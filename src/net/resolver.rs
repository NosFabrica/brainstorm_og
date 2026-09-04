//! Address filtering at DNS-resolution time, for the client that fetches
//! third-party pages.
//!
//! `validate_and_resolve` judges a host's addresses *before* the connection.
//! Between that check and the socket, a name is free to answer differently —
//! the classic DNS rebind. Filtering here removes the gap: the answer this
//! resolver returns is the set of addresses the connector may dial, so there
//! is no second, unchecked resolution.
//!
//! An IP-literal host never arrives here. The connector parses those itself
//! and skips DNS entirely, which is fine — a literal cannot rebind, and
//! `validate_url` already refuses the reserved ones.

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::net::SocketAddr;

use super::Reserved;

/// The system resolver, reached through tokio rather than hyper-util so this
/// stays a wrapper over a dependency the crate already has.
pub struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            // Port 0 on purpose: the connector rewrites it to the URL's port
            // once we return, and resolution does not depend on it.
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Drops an answer that names any address we refuse to dial.
///
/// The whole answer is rejected rather than trimmed to the survivors: a name
/// that hands back an internal address is not one we want to reach at its
/// other one, and it keeps the verdict identical to `validate_and_resolve`'s
/// on the same answer.
pub struct FilteringResolver<R> {
    inner: R,
    reserved: Reserved,
}

impl<R> FilteringResolver<R> {
    pub fn new(inner: R, reserved: Reserved) -> Self {
        Self { inner, reserved }
    }
}

/// What every deployment builds: the system resolver behind the filter.
pub fn system(reserved: Reserved) -> FilteringResolver<SystemResolver> {
    FilteringResolver::new(SystemResolver, reserved)
}

impl<R: Resolve> Resolve for FilteringResolver<R> {
    fn resolve(&self, name: Name) -> Resolving {
        let reserved = self.reserved;
        let inner = self.inner.resolve(name);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = inner.await?.collect();
            if addrs.is_empty() {
                return Err("name did not resolve".into());
            }
            // The host stays out of the message. `validate_and_resolve` does
            // name it, but that error is `anyhow` the caller can format away;
            // this one is a `reqwest` error, and those get rendered.
            if addrs.iter().any(|a| reserved.refuses(a.ip())) {
                return Err("name resolves to a reserved address".into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed(Vec<SocketAddr>);

    impl Resolve for Fixed {
        fn resolve(&self, _name: Name) -> Resolving {
            let addrs = self.0.clone();
            Box::pin(async move { Ok(Box::new(addrs.into_iter()) as Addrs) })
        }
    }

    async fn answer(addrs: &[&str], reserved: Reserved) -> Result<Vec<SocketAddr>, String> {
        let fixed = Fixed(addrs.iter().map(|a| a.parse().unwrap()).collect());
        FilteringResolver::new(fixed, reserved)
            .resolve("host.test".parse().unwrap())
            .await
            .map(|a| a.collect())
            .map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn a_reserved_answer_is_refused() {
        for addr in [
            "169.254.169.254:0",
            "127.0.0.1:0",
            "10.0.0.1:0",
            "[::1]:0",
            "[::ffff:169.254.169.254]:0",
        ] {
            assert!(
                answer(&[addr], Reserved::Refuse).await.is_err(),
                "waved through {addr}"
            );
        }
    }

    #[tokio::test]
    async fn a_public_answer_is_passed_through_unchanged() {
        let got = answer(&["1.1.1.1:0", "[2606:4700:4700::1111]:0"], Reserved::Refuse)
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
    }

    /// One bad record poisons the answer. Returning only the public address
    /// would still be safe, but it would let a name keep half a foothold and
    /// disagree with what `validate_and_resolve` says about the same answer.
    #[tokio::test]
    async fn one_reserved_address_rejects_the_whole_answer() {
        assert!(answer(&["1.1.1.1:0", "10.0.0.1:0"], Reserved::Refuse)
            .await
            .is_err());
    }

    /// An empty answer must be an error, not an empty address list the
    /// connector would report as some other failure.
    #[tokio::test]
    async fn an_empty_answer_is_an_error() {
        assert!(answer(&[], Reserved::Refuse).await.is_err());
    }

    /// The test seam is loopback and nothing more, here as everywhere else.
    #[tokio::test]
    async fn the_loopback_seam_still_refuses_everything_else() {
        assert!(answer(&["127.0.0.1:0"], Reserved::AllowLoopback)
            .await
            .is_ok());
        assert!(answer(&["[::1]:0"], Reserved::AllowLoopback).await.is_ok());
        for addr in ["169.254.169.254:0", "10.0.0.1:0", "[fd00::1]:0"] {
            assert!(
                answer(&[addr], Reserved::AllowLoopback).await.is_err(),
                "the loopback seam waved through {addr}"
            );
        }
    }
}
