//! Velm's MCP stdio server.
//!
//! `docs/07-agent-canvas.md` §1: *"`velm-mcp` — Velm's MCP stdio server, so any MCP-speaking
//! agent gets the same surface."* An agent client is configured to run this binary and then
//! talks JSON-RPC 2.0 to it over the pipe it opened, one message per line.
//!
//! # Everything is in the library, on purpose
//!
//! This file is a loop. [`vellum_agent::mcp::Server::handle_line`] takes a line and answers
//! with a line or with nothing, so every protocol decision — the handshake, the tool list, the
//! difference between a protocol error and a refused tool — is an ordinary unit test in
//! `mcp.rs` with no process, no pipe and no client. What is left here is the part a test
//! cannot reach: reading stdin, writing stdout, and the three rules below.
//!
//! # ⚠ stdout is the protocol
//!
//! Not a log, not a place to print a banner, not somewhere to report that the server started.
//! One line of anything that is not a JSON-RPC frame ends the client's session. **Every
//! diagnostic in this binary goes to stderr**, which the client shows the user and never
//! parses — and the two `eprintln!`s below are the only output this file produces that is not
//! an answer to a message.
//!
//! # Why it never exits on a bad line
//!
//! A line that is not JSON is answered with a JSON-RPC parse error and the loop continues.
//! Exiting would take the session down over one malformed frame from a client that is
//! otherwise working, and the client would report it as *"velm-mcp crashed"* — which is both
//! wrong and unactionable. The loop ends when stdin ends, which is how a client says it is
//! finished.
//!
//! ⚠ **That includes a line that is not UTF-8**, and it did not used to. `read_line` refuses
//! the whole read on one invalid byte — it answers `InvalidData` rather than the bytes — and
//! this loop treated an `Err` as end of stdin, so **a single stray byte from a client ended
//! the server**, contradicting the paragraph above. `read_until` takes the bytes as bytes and
//! `from_utf8_lossy` turns them into a line that `handle_line` can then refuse *as a protocol
//! error*, with the answer the client is waiting for. The rule generalises: at this layer an
//! `Err` should mean the pipe is gone, and nothing else.
//!
//! # A missing Velm is not a reason to refuse to start
//!
//! `VELM_IPC` is unset whenever this is run outside an agent node — from a terminal, from a
//! client the user configured by hand. That is an ordinary way to run: the research tools need
//! nothing but a socket and still work. Only the `velm_*` tools answer with a named refusal.
//! See `mcp::Endpoint`.

use std::io::{BufRead, BufReader, Write};

use vellum_agent::mcp::Server;

fn main() {
    let server = Server::new();

    // Locked once and held. Taking the lock per line would be correct and slower, and this
    // process has exactly one reader and one writer.
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    let mut raw: Vec<u8> = Vec::new();
    loop {
        raw.clear();
        // Bytes, not a `String`. See the note above: `read_line` refuses the entire read on
        // one invalid UTF-8 byte, and this loop reads an `Err` as the end of the pipe.
        match reader.read_until(b'\n', &mut raw) {
            // End of stdin: the client is finished. Not an error.
            Ok(0) => break,
            Ok(_) => {}
            Err(error) => {
                eprintln!("velm-mcp: could not read from stdin: {error}");
                break;
            }
        }
        let line = String::from_utf8_lossy(&raw);

        let Some(answer) = server.handle_line(&line) else {
            // A notification, or a blank line. Silence is the correct answer to both.
            continue;
        };

        // Flushed every frame. A client is blocked reading this pipe, so a buffered answer is
        // a deadlock rather than a delay — and `handle_line` guarantees the frame carries no
        // newline of its own, so exactly one is added here.
        if let Err(error) = writeln!(writer, "{answer}").and_then(|()| writer.flush()) {
            eprintln!("velm-mcp: could not write to stdout: {error}");
            break;
        }
    }
}
