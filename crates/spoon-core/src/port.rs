//! Server port assignment: picking a free port for a new server.

use std::net::TcpListener;

/// The default Minecraft server port.
pub const DEFAULT_SERVER_PORT: u16 = 25565;

/// Best-effort check that nothing on this machine is currently listening on
/// `port` (binding `0.0.0.0` like a Minecraft server does by default).
pub fn is_port_available(port: u16) -> bool {
    TcpListener::bind(("0.0.0.0", port)).is_ok()
}

/// Lowest port starting at `start` that no other Spoon server has assigned.
/// Used to compute the suggested port before probing the OS; see
/// [`next_available_port`].
pub fn next_unassigned_port(start: u16, assigned: &[u16]) -> u16 {
    let mut port = start;
    while assigned.contains(&port) {
        match port.checked_add(1) {
            Some(next) => port = next,
            None => break,
        }
    }
    port
}

/// Lowest port starting at `start` that is neither in `assigned` nor
/// currently bound on this machine.
pub fn next_available_port(start: u16, assigned: &[u16]) -> u16 {
    let mut port = next_unassigned_port(start, assigned);
    while !is_port_available(port) {
        match port.checked_add(1) {
            Some(next) => port = next,
            None => break,
        }
    }
    port
}

#[cfg(test)]
mod tests {
    use super::{next_unassigned_port, DEFAULT_SERVER_PORT};

    #[test]
    fn keeps_default_port_when_free() {
        assert_eq!(next_unassigned_port(DEFAULT_SERVER_PORT, &[]), 25565);
        assert_eq!(next_unassigned_port(DEFAULT_SERVER_PORT, &[25566]), 25565);
    }

    #[test]
    fn increases_one_by_one_past_assigned_ports() {
        assert_eq!(next_unassigned_port(25565, &[25565]), 25566);
        assert_eq!(next_unassigned_port(25565, &[25565, 25566]), 25567);
        // Order of the assigned list should not matter.
        assert_eq!(next_unassigned_port(25565, &[25566, 25565]), 25567);
    }

    #[test]
    fn stops_at_the_highest_port() {
        assert_eq!(next_unassigned_port(u16::MAX, &[u16::MAX]), u16::MAX);
    }
}
