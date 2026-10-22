import importlib.machinery, importlib.util, sys, io, contextlib, argparse
import xml.etree.ElementTree as ET
path = sys.argv[1]
loader = importlib.machinery.SourceFileLoader('nc', path)
spec = importlib.util.spec_from_loader('nc', loader); nc = importlib.util.module_from_spec(spec); loader.exec_module(nc)
W = lambda body: ET.fromstring(f'<ApiResponse xmlns="http://api.namecheap.com/xml.response" Status="OK"><CommandResponse>{body}</CommandResponse></ApiResponse>').find(nc.NS+'CommandResponse')
calls = []
R = {
 'namecheap.users.getBalances': W('<UserGetBalancesResult Currency="USD" AvailableBalance="12.00" AccountBalance="12.00" EarnedAmount="0" FundsRequiredForAutoRenew="0"/>'),
 'namecheap.domains.check': W('<DomainCheckResult Domain="new.dev" Available="true" IsPremiumName="false"/><DomainCheckResult Domain="google.com" Available="false" IsPremiumName="false"/>'),
 'namecheap.users.getPricing': W('<UserGetPricingResult><ProductType Name="domains"><ProductCategory Name="register"><Product Name="dev"><Price Duration="1" DurationType="YEAR" Price="12.98" YourPrice="11.98" RegularPrice="15.98" Currency="USD"/><Price Duration="2" Price="25" Currency="USD"/></Product></ProductCategory></ProductType></UserGetPricingResult>'),
 'namecheap.domains.getInfo': W('<DomainGetInfoResult Status="Ok" DomainName="a.com" OwnerName="me" IsPremium="false"><DomainDetails><CreatedDate>01/01/2024</CreatedDate><ExpiredDate>01/01/2027</ExpiredDate></DomainDetails><Whoisguard Enabled="True"><ExpiredDate>01/01/2027</ExpiredDate></Whoisguard><DnsDetails ProviderType="FREE"><Nameserver>dns1.registrar-servers.com</Nameserver><Nameserver>dns2.registrar-servers.com</Nameserver></DnsDetails></DomainGetInfoResult>'),
 'namecheap.domains.getRegistrarLock': W('<DomainGetRegistrarLockResult Domain="a.com" RegistrarLockStatus="true"/>'),
 'namecheap.domains.getContacts': W('<DomainContactsResult>' + ''.join(f'<{r}><FirstName>V</FirstName><LastName>D</LastName><EmailAddress>v@x.com</EmailAddress><Country>US</Country></{r}>' for r in nc.CONTACT_ROLES) + '</DomainContactsResult>'),
 'namecheap.domains.dns.getList': W('<DomainDNSGetListResult Domain="a.com" IsUsingOurDNS="true"><Nameserver>dns1.registrar-servers.com</Nameserver></DomainDNSGetListResult>'),
}
pages = {1: W('<DomainGetListResult>' + ''.join(f'<Domain Name="d{i}.com" Expires="01/01/2027" IsOurDNS="true" AutoRenew="true" IsLocked="false" WhoisGuard="ENABLED"/>' for i in range(100)) + '</DomainGetListResult><Paging><TotalItems>130</TotalItems></Paging>'),
         2: W('<DomainGetListResult>' + ''.join(f'<Domain Name="e{i}.com" Expires="01/01/2027" IsOurDNS="false"/>' for i in range(30)) + '</DomainGetListResult><Paging><TotalItems>130</TotalItems></Paging>')}
def call(creds, command, **params):
    calls.append((command, params))
    if command == 'namecheap.domains.getList': return pages[params['Page']]
    if command in R: return R[command]
    raise AssertionError('unexpected spend/write call: ' + command)
nc.call = call
nc.load_creds = lambda: {'ApiUser':'u','ApiKey':'k','UserName':'u','ClientIp':'1.2.3.4','sandbox':False}
def run(fn, **kw):
    out = io.StringIO()
    code = None
    with contextlib.redirect_stdout(out):
        try: fn(argparse.Namespace(**kw))
        except SystemExit as e: code = str(e)
    return out.getvalue(), code
o,_ = run(nc.cmd_domains, expiring=False, expired=False, search=None); assert o.startswith('130 domain(s)') and 'auto-renew privacy' in o, o
o,_ = run(nc.cmd_info, domain='a.com'); assert 'expires 01/01/2027' in o and 'registrar lock true' in o and 'dns1.registrar-servers.com' in o, o
o,_ = run(nc.cmd_available, domains=['new.dev','google.com']); assert 'new.dev' in o and 'AVAILABLE' in o and 'taken' in o, o
o,_ = run(nc.cmd_price, tld='.dev', action='register', years=1); assert '11.98 USD' in o, o
o,c = run(nc.cmd_renew, domain='a.dev', years=1, yes=False); assert c and 'Not charged' in c and not any(x[0]=='namecheap.domains.renew' for x in calls), (o,c)
o,c = run(nc.cmd_register, domain='new.dev', contacts_from='a.com', years=1, nameservers=None, yes=False); assert 'Not charged' in c and '11.98' in o, (o,c)
o,c = run(nc.cmd_nameservers, domain='a.com', set=['ns1.x.com','ns2.x.com'], default=False, yes=False); assert 'Not changed' in c, c
o,_ = run(nc.cmd_nameservers, domain='a.com', set=None, default=False, yes=False); assert 'Namecheap DNS' in o, o
o,_ = run(nc.cmd_contacts, domain='a.com'); assert 'Registrant' in o and 'v@x.com' in o, o
# register with --yes builds full contact params
R['namecheap.domains.create'] = W('<DomainCreateResult Domain="new.dev" Registered="true" ChargedAmount="11.98" OrderID="1"/>')
o,c = run(nc.cmd_register, domain='new.dev', contacts_from='a.com', years=1, nameservers=None, yes=True); assert c is None and 'Registered new.dev' in o, (o,c)
params = [p for cmd,p in calls if cmd=='namecheap.domains.create'][0]
assert params['RegistrantFirstName']=='V' and params['AuxBillingEmailAddress']=='v@x.com' and params['WGEnabled']=='yes'
print('ALL OK', len(calls), 'calls')
