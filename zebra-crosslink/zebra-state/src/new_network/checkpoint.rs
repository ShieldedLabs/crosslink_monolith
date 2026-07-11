// Lite checkpoint: the synced chain must contain `hash` at `height`

use crate::config::NetworkCheckpoint;
use zebra_chain::block::Hash;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    pub height: u32,
    pub hash: Hash,
}

impl Checkpoint {
    pub fn from_config(config: &Option<NetworkCheckpoint>) -> Option<Checkpoint> {
        config.map(|(height, hash)| Checkpoint { height, hash })
    }

    // true iff a block with this height/hash can never be on the checkpointed chain
    pub fn rejects(&self, height: u32, hash: Hash) -> bool {
        height == self.height && hash != self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_HEX: &str = "02fc9aa7652b2a9a9f6196a446265fed6012227373ed34e0b17c7a3298db005e";

    fn cp() -> Checkpoint {
        Checkpoint { height: 100, hash: HASH_HEX.parse().unwrap() }
    }

    #[test]
    fn rejects_only_wrong_hash_at_checkpoint_height() {
        let cp = cp();
        let other = Hash([0x42; 32]);
        assert!(!cp.rejects(99, other)); // below: anything goes
        assert!(!cp.rejects(101, other)); // above: anything goes
        assert!(!cp.rejects(100, cp.hash)); // the checkpoint block itself
        assert!(cp.rejects(100, other)); // wrong block at the pinned height
    }

    #[test]
    fn from_config_states() {
        let pin: NetworkCheckpoint = (100, HASH_HEX.parse().unwrap());
        assert_eq!(Checkpoint::from_config(&Some(pin)), Some(cp()));
        assert_eq!(Checkpoint::from_config(&None), None);
    }

    #[test]
    fn config_toml_parses_hash_in_display_order() {
        let toml = format!("[network_checkpoint]\nheight = 100\nhash = \"{HASH_HEX}\"");
        let config: crate::config::Config = toml::from_str(&toml).unwrap();
        assert_eq!(Checkpoint::from_config(&config.network_checkpoint), Some(cp()));
    }

    #[test]
    fn config_toml_disabled_is_exactly_height_0_hash_empty() {
        let toml = "[network_checkpoint]\nheight = 0\nhash = \"\"";
        let config: crate::config::Config = toml::from_str(toml).unwrap();
        assert_eq!(config.network_checkpoint, None);
        assert_eq!(Checkpoint::from_config(&config.network_checkpoint), None);
    }

    #[test]
    fn bad_checkpoint_rejects_whole_config() {
        let bad = [
            "[network_checkpoint]\nheight = 100\nhash = \"not hex\"".to_string(),
            "[network_checkpoint]\nheight = 100\nhash = \"02fc9aa7\"".to_string(), // wrong length
            "[network_checkpoint]\nheight = 100".to_string(), // missing hash
            format!("[network_checkpoint]\nhash = \"{HASH_HEX}\""), // missing height
            format!("[network_checkpoint]\nheigth = 100\nhash = \"{HASH_HEX}\""), // typo'd field
            "[network_checkpoint]\nheight = 100\nhash = \"\"".to_string(), // empty hash, nonzero height
            format!("[network_checkpoint]\nheight = 0\nhash = \"{HASH_HEX}\""), // zero height, real hash
        ];
        for toml in &bad {
            assert!(toml::from_str::<crate::config::Config>(toml).is_err(), "should reject: {toml}");
        }
    }

    #[test]
    fn checkpoint_toml_round_trips() {
        #[derive(serde::Serialize, serde::Deserialize)]
        struct W {
            #[serde(with = "crate::config::network_checkpoint_toml")]
            network_checkpoint: Option<NetworkCheckpoint>,
        }
        let pin: NetworkCheckpoint = (100, HASH_HEX.parse().unwrap());
        for v in [None, Some(pin)] {
            let s = toml::to_string(&W { network_checkpoint: v }).unwrap();
            let w: W = toml::from_str(&s).unwrap();
            assert_eq!(w.network_checkpoint, v);
        }
    }
}
