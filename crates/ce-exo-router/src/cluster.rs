//! Forming an exo cluster across NAT using CE tunnels.
//!
//! exo's nodes discover and stream to each other over the network; across CE's NAT'd mesh they can't
//! see each other directly. This module opens CE `tunnel`s (the raw-stream transport primitive) so
//! each member reaches its peers at `127.0.0.1:<port>`, and produces the peer list to hand exo as
//! **manual discovery**. CE provides the NAT traversal + capability auth; exo does the inference.
//!
//! This is the genuine CE value-add over plain exo: machines that cannot directly route to each
//! other (laptop behind NAT, desktop behind NAT, relay in the cloud) form one exo ring over the
//! mesh, and only capability-holders can join.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// One CE tunnel to open in the local node: bind `127.0.0.1:local_port` and forward to
/// `target_node`:`remote_port` over the mesh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelSpec {
    pub target_node: String,
    pub local_port: u16,
    pub remote_port: u16,
    /// Hex `ce-cap` token granting the `tunnel` ability to the target (`None` relies on a self-rooted
    /// grant the local node already holds).
    pub caps: Option<String>,
}

/// Open one tunnel by POSTing to the local CE node's `/tunnel` endpoint. `node_base` is the node API
/// base URL (e.g. `http://127.0.0.1:8844`); `token` is its API bearer token.
pub async fn open_tunnel(node_base: &str, token: Option<&str>, spec: &TunnelSpec) -> Result<()> {
    let mut req = reqwest::Client::new().post(format!("{}/tunnel", node_base.trim_end_matches('/')));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let body = serde_json::json!({
        "node_id": spec.target_node,
        "local_port": spec.local_port,
        "remote_port": spec.remote_port,
        "caps": spec.caps,
    });
    let resp = req.json(&body).send().await.context("calling /tunnel")?;
    if !resp.status().is_success() {
        let status = resp.status();
        return Err(anyhow!("tunnel failed ({status}): {}", resp.text().await.unwrap_or_default()));
    }
    Ok(())
}

/// A member of an exo cluster: its CE node id and the exo inter-node (peer) port it listens on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExoMember {
    pub node_id: String,
    /// exo's inter-node peer port on that machine.
    pub peer_port: u16,
}

/// The local wiring for one machine to join an exo cluster: the tunnels to open and the peer
/// endpoints (`127.0.0.1:<port>`) to hand exo as manual discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalWiring {
    pub tunnels: Vec<TunnelSpec>,
    /// Peer endpoints exo should dial — each a local port mapped to a remote member by a tunnel.
    pub peers: Vec<String>,
}

/// Plan the tunnels + discovery for `self_node` to reach every other member. Local ports are
/// allocated sequentially from `base_local_port`. `caps` (if any) authorizes each tunnel.
pub fn wire_local(
    self_node: &str,
    members: &[ExoMember],
    base_local_port: u16,
    caps: Option<&str>,
) -> LocalWiring {
    let mut tunnels = Vec::new();
    let mut peers = Vec::new();
    let mut port = base_local_port;
    for m in members {
        if m.node_id == self_node {
            continue;
        }
        tunnels.push(TunnelSpec {
            target_node: m.node_id.clone(),
            local_port: port,
            remote_port: m.peer_port,
            caps: caps.map(String::from),
        });
        peers.push(format!("127.0.0.1:{port}"));
        port += 1;
    }
    LocalWiring { tunnels, peers }
}

/// Open every tunnel in `wiring` against the local node. Returns the peer endpoints exo should use.
pub async fn connect_local(
    node_base: &str,
    token: Option<&str>,
    wiring: &LocalWiring,
) -> Result<Vec<String>> {
    for t in &wiring.tunnels {
        open_tunnel(node_base, token, t).await?;
    }
    Ok(wiring.peers.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wires_to_other_members_only() {
        let members = vec![
            ExoMember { node_id: "self".into(), peer_port: 5001 },
            ExoMember { node_id: "b".into(), peer_port: 5001 },
            ExoMember { node_id: "c".into(), peer_port: 5002 },
        ];
        let w = wire_local("self", &members, 6000, None);
        assert_eq!(w.tunnels.len(), 2);
        assert_eq!(w.peers, vec!["127.0.0.1:6000", "127.0.0.1:6001"]);
        assert_eq!(w.tunnels[0].target_node, "b");
        assert_eq!(w.tunnels[0].remote_port, 5001);
        assert_eq!(w.tunnels[1].target_node, "c");
        assert_eq!(w.tunnels[1].local_port, 6001);
    }

    #[test]
    fn caps_propagate_to_each_tunnel() {
        let members = vec![ExoMember { node_id: "b".into(), peer_port: 9000 }];
        let w = wire_local("self", &members, 6000, Some("deadbeef"));
        assert_eq!(w.tunnels[0].caps.as_deref(), Some("deadbeef"));
    }
}
