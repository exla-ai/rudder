import {check} from "./types.js";

/** Fixed public inputs of the actual image-data builder. This deliberately
 * grants no general /etc scope and follows its raw UTF-8 byte ordering. */
export function imageManifestPathAfter(value:unknown,previous:Buffer):Buffer {
 check(typeof value==="string","invalid_request");
 const bytes=Buffer.from(value,"utf8");
 check(bytes.toString("utf8")===value&&Buffer.compare(bytes,previous)>0&&
  (/^(?:\/usr\/|\/opt\/rudder(?:\/|-managed-runtime\/)|\/etc\/alternatives\/|\/etc\/ssl\/certs\/)/.test(value)||
   value==="/etc/ld.so.cache"||value==="/etc/localtime"||value==="/etc/ssl/openssl.cnf"||value==="/etc/python3.11/sitecustomize.py")&&
  !value.includes("\0")&&!value.split("/").slice(1).some(part=>part===""||part==="."||part===".."),"invalid_request");
 return bytes;
}
