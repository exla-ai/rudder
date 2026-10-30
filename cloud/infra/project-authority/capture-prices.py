#!/usr/bin/env python3
"""Read public AWS price catalogs only. No SDK, account API, credentials or deploy.

Writes selected published rates plus whole-catalog digests. The EC2 catalog is
streamed in memory; no large catalog or environment/profile data is saved.
"""
import concurrent.futures
import csv
import datetime
import hashlib
import io
import json
import pathlib
import urllib.request

REGION = "us-east-1"
BASE = "https://pricing.us-east-1.amazonaws.com/offers/v1.0/aws"
SELECT = {
    "AmazonRDS": {
        "5SY529FVXY3EARD8",  # PostgreSQL db.t4g.small, traditional Multi-AZ
        "Z8HB5KU2V54F7ATT",  # PostgreSQL db.m7g.large, traditional Multi-AZ
        "J7S7KD4WFDNQWKNX",  # PostgreSQL Multi-AZ gp3
        "DXW9ERDR4STYT9D7",  # PostgreSQL T4g surplus CPU credits
        "6W8ECRFVDATCER7J",  # PostgreSQL charged backup storage
        "AVKW7ZJP8D72HNQC",  # Aurora PostgreSQL Serverless v2 Standard ACU
        "7GYUVAA992PFJPCB",  # Aurora PostgreSQL Standard storage
        "7JDSZP38DMKT9B86",  # Aurora PostgreSQL Standard I/O
    },
    "AmazonVPC": {"EN2N5TATXE673A3B", "4GQUNXTFWVSGPUZK"},
    "AWSSecretsManager": {"BJ3PQ9BYGU6P632F", "4MDZ5VNEJPMUTG9B"},
    "awskms": {"U553K98XGDXCYHWS", "MFEBZPX8NHM5FY7Z"},
    "AmazonApiGateway": {"FC2TWT2UEPTBKVBX"},
    "AWSLambda": {"TG3M4CAGBA3NYQBH", "GU2ZS9HVP6QTQ7KE"},
}


def json_catalog(service):
    url = f"{BASE}/{service}/current/{REGION}/index.json"
    with urllib.request.urlopen(url, timeout=60) as response:
        raw = response.read()
    data = json.loads(raw)
    selected = set(SELECT[service])
    if not SELECT[service].issubset(data["products"]):
        raise ValueError(f"Published SKUs changed for {service}; review selection")
    rows = []
    for sku in sorted(selected):
        product = data["products"][sku]
        prices = [
            dict(rateCode=dimension["rateCode"], unit=dimension["unit"],
                 begin=dimension["beginRange"], end=dimension["endRange"],
                 usd=dimension["pricePerUnit"]["USD"],
                 description=dimension["description"], effectiveDate=term["effectiveDate"])
            for term in data["terms"].get("OnDemand", {}).get(sku, {}).values()
            for dimension in term["priceDimensions"].values()
        ]
        if not prices:
            raise ValueError(f"No on-demand price for {service}/{sku}")
        rows.append(dict(sku=sku, productFamily=product.get("productFamily"),
                         attributes=product["attributes"], prices=prices))
    return dict(service=service, url=url, version=data["version"],
                publicationDate=data["publicationDate"], bytes=len(raw),
                sha256=hashlib.sha256(raw).hexdigest(), selected=rows)


class DigestReader(io.RawIOBase):
    def __init__(self, response):
        self.response = response
        self.digest = hashlib.sha256()
        self.count = 0

    def readable(self):
        return True

    def readinto(self, buffer):
        chunk = self.response.read(len(buffer))
        buffer[:len(chunk)] = chunk
        self.digest.update(chunk)
        self.count += len(chunk)
        return len(chunk)


def ec2_catalog():
    url = f"{BASE}/AmazonEC2/current/{REGION}/index.csv"
    rows = []
    with urllib.request.urlopen(url, timeout=60) as response:
        reader = DigestReader(response)
        text = io.TextIOWrapper(io.BufferedReader(reader), encoding="utf-8", newline="")
        metadata = dict(next(csv.reader([text.readline()])) for _ in range(5))
        for row in csv.DictReader(text):
            compute = (row["Instance Type"] == "t4g.nano"
                       and row["Operating System"] == "Linux"
                       and row["Tenancy"] == "Shared"
                       and row["Pre Installed S/W"] == "NA"
                       and row["CapacityStatus"] == "Used"
                       and row["operation"] == "RunInstances")
            storage = row["usageType"] == "EBS:VolumeUsage.gp3"
            if row["TermType"] == "OnDemand" and (compute or storage):
                rows.append({key: value for key, value in row.items() if value})
        digest, count = reader.digest.hexdigest(), reader.count
    if len(rows) != 2:
        raise ValueError("EC2 published matching products changed; review selection")
    return dict(service="AmazonEC2", url=url, version=metadata["Version"],
                publicationDate=metadata["Publication Date"], bytes=count,
                sha256=digest, selected=rows)


if __name__ == "__main__":
    with concurrent.futures.ThreadPoolExecutor(max_workers=7) as executor:
        jobs = [executor.submit(json_catalog, service) for service in SELECT]
        jobs.append(executor.submit(ec2_catalog))
        catalogs = [job.result() for job in jobs]
    result = dict(
        capturedAt=datetime.datetime.now(datetime.timezone.utc).isoformat(),
        region=REGION, currency="USD", monthlyHours=730,
        scope="Public on-demand catalogs; no account, discount, tax or spend API",
        catalogs=catalogs,
    )
    output = pathlib.Path(__file__).with_name("pricing-us-east-1.json")
    output.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(dict(path=str(output), catalogs=len(catalogs),
                          selectedRates=sum(len(c["selected"]) for c in catalogs))))
