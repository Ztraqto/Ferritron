//! `ferritron-mcp`: the MCP server to configure in AI clients, e.g. in
//! Claude Code: `claude mcp add ferritron -- ferritron-mcp`.
//!
//! The first one on a computer becomes the hub; later ones relay to it.

use ferritron_mcp::{Bridge, Hub, ensure_token, serve_stdio};

fn main() {
    let port = ferritron_core::mcp::port();
    let Some(path) = ferritron_core::mcp::token_path() else {
        eprintln!("ferritron-mcp: cannot find the Ferritron data folder");
        std::process::exit(1);
    };
    let token = match ensure_token(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ferritron-mcp: cannot read or create {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    match Hub::start(port, token.clone()) {
        Ok(hub) => serve_stdio(&|name, args| hub.call_tool(name, args)),
        Err(_) => match Bridge::connect(port, &token) {
            // Another server is the hub: relay to it.
            Ok(bridge) => serve_stdio(&|name, args| bridge.call_tool(name, args)),
            Err(e) => {
                eprintln!("ferritron-mcp: port {port} is taken and no Ferritron hub answers there: {e}");
                std::process::exit(1);
            }
        },
    }
}
