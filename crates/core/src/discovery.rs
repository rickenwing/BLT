//! mDNS / DNS-SD discovery — the pure, testable half.
//!
//! Service type `_blt._tcp` (TDD §9). **TXT records carry `IP:port`, not
//! `.local` hostnames** (HARD CONSTRAINT / F15 #9) so clients use mDNS only for
//! service *discovery*, never hostname *resolution* — avoiding flaky `.local`
//! resolution on older Windows. The live `mdns-sd` daemon glue lives in the
//! server/desktop crates; this module owns the TXT contract and parsing.

use std::collections::HashMap;

/// The advertised service type.
pub const SERVICE_TYPE: &str = "_blt._tcp.local.";

/// TXT keys.
pub const TXT_UUID: &str = "uuid";
pub const TXT_LABEL: &str = "label";
pub const TXT_GAME: &str = "game";
pub const TXT_SHARE: &str = "share";

/// What the server advertises (the admin panel is intentionally not advertised).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertisedServer {
    pub uuid: String,
    pub label: String,
    /// `IP:port` of the game-distribution listener's bound NIC.
    pub game_endpoint: String,
    /// `IP:port` of the shared-pool listener's bound NIC.
    pub share_endpoint: String,
}

/// What a client resolves from a discovered service's TXT records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredServer {
    pub uuid: String,
    pub label: String,
    pub game_endpoint: String,
    pub share_endpoint: String,
}

/// Build the TXT property map for advertising.
pub fn build_txt(s: &AdvertisedServer) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert(TXT_UUID.to_string(), s.uuid.clone());
    m.insert(TXT_LABEL.to_string(), s.label.clone());
    m.insert(TXT_GAME.to_string(), s.game_endpoint.clone());
    m.insert(TXT_SHARE.to_string(), s.share_endpoint.clone());
    m
}

/// Parse TXT properties into a [`DiscoveredServer`]. Returns `None` unless the
/// essential `uuid` + `game` endpoint are present (a server without a reachable
/// game endpoint is not useful). Validates endpoints look like `host:port`.
pub fn parse_txt(props: &HashMap<String, String>) -> Option<DiscoveredServer> {
    let uuid = props.get(TXT_UUID)?.clone();
    let game = props.get(TXT_GAME).cloned().filter(|e| is_endpoint(e))?;
    let share = props
        .get(TXT_SHARE)
        .cloned()
        .filter(|e| is_endpoint(e))
        .unwrap_or_default();
    let label = props
        .get(TXT_LABEL)
        .cloned()
        .unwrap_or_else(|| uuid.clone());
    Some(DiscoveredServer {
        uuid,
        label,
        game_endpoint: game,
        share_endpoint: share,
    })
}

/// Cheap `host:port` shape check (the port must parse; the host non-empty).
/// Deliberately accepts IPv4/hostname; we expect an IP per #9 but don't reject
/// a hostname outright.
pub fn is_endpoint(s: &str) -> bool {
    match s.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && port.parse::<u16>().is_ok(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AdvertisedServer {
        AdvertisedServer {
            uuid: "abc-123".into(),
            label: "Rick's LAN".into(),
            game_endpoint: "192.168.1.10:7400".into(),
            share_endpoint: "192.168.1.11:7401".into(),
        }
    }

    #[test]
    fn txt_roundtrip() {
        let s = sample();
        let txt = build_txt(&s);
        let parsed = parse_txt(&txt).unwrap();
        assert_eq!(parsed.uuid, s.uuid);
        assert_eq!(parsed.label, s.label);
        assert_eq!(parsed.game_endpoint, s.game_endpoint);
        assert_eq!(parsed.share_endpoint, s.share_endpoint);
    }

    #[test]
    fn endpoints_are_ip_port_not_dot_local() {
        let txt = build_txt(&sample());
        // #9: advertised endpoints are IP:port, never a `.local` name.
        assert!(!txt[TXT_GAME].contains(".local"));
        assert!(is_endpoint(&txt[TXT_GAME]));
    }

    #[test]
    fn parse_requires_uuid_and_game() {
        let mut m = HashMap::new();
        assert!(parse_txt(&m).is_none());
        m.insert(TXT_UUID.into(), "u".into());
        assert!(parse_txt(&m).is_none()); // no game endpoint
        m.insert(TXT_GAME.into(), "10.0.0.1:7400".into());
        let d = parse_txt(&m).unwrap();
        assert_eq!(d.label, "u"); // label falls back to uuid
        assert_eq!(d.share_endpoint, ""); // share optional
    }

    #[test]
    fn rejects_malformed_endpoint() {
        assert!(!is_endpoint("noport"));
        assert!(!is_endpoint(":7400"));
        assert!(!is_endpoint("host:notaport"));
        assert!(is_endpoint("host:7400"));
        assert!(is_endpoint("192.168.0.5:1"));
    }
}
