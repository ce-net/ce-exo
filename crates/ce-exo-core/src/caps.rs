//! Capability abilities — ce-exo's authorization vocabulary.
//!
//! ce-exo adds no node trust of its own: every privileged action is gated by a `ce-cap` capability
//! chain rooted at the worker's own key or a configured root. Abilities are opaque strings (the CE
//! convention); ce-exo reserves the `exo:` namespace. A request carries its chain; the worker
//! verifies it names the required ability before acting.
//!
//! The per-model restriction rides `ce-cap` attenuation as a structured ability rather than a custom
//! caveat: `exo:model:<prefix>` authorizes only model ids beginning with `<prefix>`. A grant of
//! `exo:infer` + `exo:model:llama3` lets the holder run any `llama3*` model but nothing else.

/// Send inference requests (chat / completion / embedding) to a worker.
pub const INFER: &str = "exo:infer";

/// Act as an inference host: accept being placed as a replica or a pipeline stage.
pub const HOST: &str = "exo:host";

/// Participate as a pipeline stage (receive activations from a peer stage and forward them).
pub const SHARD: &str = "exo:shard";

/// Administer the fleet: publish models, push placements, drain/reroute hosts.
pub const ADMIN: &str = "exo:admin";

/// Build the structured per-model-prefix ability for a grant, e.g. `exo:model:llama3`.
pub fn model_prefix_ability(prefix: &str) -> String {
    format!("exo:model:{prefix}")
}

/// Given the abilities a verified chain grants, does it permit running `model_id`?
///
/// A chain with no `exo:model:*` ability is unrestricted (any model). If one or more are present,
/// the model id must match at least one prefix. This is enforced *in addition* to the action ability
/// ([`INFER`] / [`SHARD`]) by the worker at the leaf.
pub fn model_allowed<'a>(abilities: impl IntoIterator<Item = &'a str>, model_id: &str) -> bool {
    let mut saw_restriction = false;
    for a in abilities {
        if let Some(prefix) = a.strip_prefix("exo:model:") {
            saw_restriction = true;
            if model_id.starts_with(prefix) {
                return true;
            }
        }
    }
    !saw_restriction
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrestricted_chain_allows_any_model() {
        assert!(model_allowed(["exo:infer"], "llama3.1-8b"));
        assert!(model_allowed([], "anything"));
    }

    #[test]
    fn model_prefix_restricts() {
        let abilities = ["exo:infer", "exo:model:llama3"];
        assert!(model_allowed(abilities, "llama3.1-8b-q4"));
        assert!(!model_allowed(abilities, "qwen2-7b"));
    }

    #[test]
    fn multiple_prefixes_are_union() {
        let abilities = ["exo:model:llama3", "exo:model:qwen2"];
        assert!(model_allowed(abilities, "qwen2-7b"));
        assert!(model_allowed(abilities, "llama3.1-8b"));
        assert!(!model_allowed(abilities, "mistral-7b"));
    }

    #[test]
    fn builds_prefix_ability() {
        assert_eq!(model_prefix_ability("clinical-"), "exo:model:clinical-");
    }
}
