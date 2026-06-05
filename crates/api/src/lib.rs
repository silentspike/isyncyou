//! `isyncyou-api` — shared boundary for the local API.
//!
//! The current localhost/Unix-socket adapter lives in `gui/webui`; see
//! `docs/local-api-security.md` for the shipped local boundary and the remaining
//! remote-admin hardening work.

#[cfg(test)]
mod tests {
    #[test]
    fn smoke() {
        assert_eq!(2 + 2, 4);
    }
}
