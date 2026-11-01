// Pure bounded byte custody. No socket, credential, authority or reset factory.
export class PacketQueue {
  #items=[];#unknown=[];
  constructor(limit=8,bytes=32768){this.limit=limit;this.bytes=bytes;}
  push(value){const bytes=Buffer.from(JSON.stringify(value));if(!bytes.length||bytes.length>this.bytes||this.#items.length+this.#unknown.length>=this.limit)throw Error('PACKET_BOUND');this.#items.push(bytes);}
  flush(write){if(!this.#items.length)return false;const original=this.#items[0];try{const count=write(original);if(count!==original.length){this.#unknown.push(original);this.#items.shift();throw Error('PACKET_SEND_UNKNOWN');}this.#items.shift();return true;}catch(error){if(error?.code==='EAGAIN'||error?.code==='EWOULDBLOCK')return false;if(this.#items[0]===original){this.#unknown.push(original);this.#items.shift();}throw error;}}
  get length(){return this.#items.length;}
  get unknown(){return this.#unknown.length;}
  snapshot(){return [...this.#items,...this.#unknown].map(bytes=>Buffer.from(bytes));}
}
export const sameGeneration=(record,value)=>record&&value&&record.serial===value.serial&&record.generation===value.generation&&record.nonce===value.attempt_nonce;
export function canonicalData(value){if(Array.isArray(value))return value.map(canonicalData);if(value&&typeof value==='object')return Object.fromEntries(Object.keys(value).sort((a,b)=>Buffer.compare(Buffer.from(a),Buffer.from(b))).map(key=>[key,canonicalData(value[key])]));return value;}
