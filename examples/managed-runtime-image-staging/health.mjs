// Fixed staging TLS bootstrap liveness only. No DB/provider/credential/Root
// ready authority. The exact child is settled before the real driver takes8080.
import http from 'node:http';
if(process.argv.length!==2||Object.keys(process.env).length)throw Error('staging health fixed invocation');
http.createServer((req,res)=>{if(req.method==='GET'&&req.url==='/health'){res.writeHead(200,{'content-type':'application/json'});res.end('{"scope":"staging-tls-liveness-only"}\n');}else{res.writeHead(404);res.end();}}).listen(8080,'0.0.0.0');
