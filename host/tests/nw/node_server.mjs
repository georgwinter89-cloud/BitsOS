// NW-10: the node.js partner (FD-1 side finding 5: node delivered 3-5 MB/s into BitsOS, python's http.server
// 116 MB/s). Serves the test file the way node programs usually do: a read stream piped into the response
// (64 KiB pieces, http.Server's defaults: keep-alive, Nagle off).
//
// GET /file         -> the file, Content-Length, piped from a read stream
// GET /chunked      -> the same pipe without a length (node sends it chunked)
// GET /file1m       -> as /file, but the read stream reads 1 MiB at a time (highWaterMark)
// GET /buffer       -> the file from memory in one res.end (Content-Length)
// GET /file.sha256  -> "<sha256 hex> <size>\n"
// anything else     -> 404
//
// Usage: node node_server.mjs <port> <file> [bind address, default 127.0.0.1]
import { createReadStream, readFileSync, statSync } from "node:fs";
import { createHash } from "node:crypto";
import { createServer } from "node:http";

const [port, file, bind = "127.0.0.1"] = process.argv.slice(2);
const size = statSync(file).size;
const data = readFileSync(file);
const sha = createHash("sha256").update(data).digest("hex");

createServer((req, res) => {
    const path = req.url;
    if (path === "/file.sha256") {
        res.end(`${sha} ${size}\n`);
    } else if (path === "/file" || path === "/chunked" || path === "/file1m") {
        res.setHeader("Content-Type", "application/octet-stream");
        if (path !== "/chunked") res.setHeader("Content-Length", size);
        createReadStream(file, path === "/file1m" ? { highWaterMark: 1 << 20 } : {}).pipe(res);
    } else if (path === "/buffer") {
        res.setHeader("Content-Length", size);
        res.end(data);
    } else {
        res.statusCode = 404;
        res.end("not found\n");
    }
    // The guest resets on purpose (its size limit test): not an error worth a crash.
    res.on("error", () => {});
}).listen(Number(port), bind);
