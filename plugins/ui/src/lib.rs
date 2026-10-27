//! `simpletickets-ui`: a local web server that serves a kanban board and relays its
//! requests to the SimpleTickets daemon, adding the API token on the server side.

pub mod client;
pub mod guard;
pub mod server;
