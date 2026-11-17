#![warn(unused_crate_dependencies)]
#![warn(missing_docs)]
//! # Neolink-Core
//!
//! Neolink-Core is a rust library for interacting with reolink and family cameras.
//!
//! Most high level camera controls are in the [`bc_protocol`] module
//!
//! A camera can be initialised with
//!
//! ```no_run
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! use neolink_core::bc_protocol::{BcCamera, BcCameraOpt, DiscoveryMethods, ConnectionProtocol, Credentials};
//! let options = BcCameraOpt {
//!     name: "CamName".to_string(),
//!     channel_id: 0,
//!     addrs: ["192.168.1.1".parse().unwrap()].to_vec(),
//!     port: Some(9000),
//!     uid: Some("CAMUID".to_string()),
//!     protocol: ConnectionProtocol::TcpUdp,
//!     discovery: DiscoveryMethods::Relay,
//!     credentials: Credentials {
//!         username: "username".to_string(),
//!         password: Some("password".to_string()),
//!     },
//!     debug: false,
//!     max_discovery_retries: 10,
//! };
//! let mut camera = BcCamera::new(&options).await.unwrap();
//! # })
//! ```
//!
//! After that login can be conducted with
//!
//! ```no_run
//! # tokio::runtime::Runtime::new().unwrap().block_on(async {
//! # use neolink_core::bc_protocol::{BcCamera, BcCameraOpt, DiscoveryMethods, ConnectionProtocol, Credentials};
//! # let options = BcCameraOpt {
//! #    name: "CamName".to_string(),
//! #    channel_id: 0,
//! #    addrs: ["192.168.1.1".parse().unwrap()].to_vec(),
//! #    port: Some(9000),
//! #    uid: Some("CAMUID".to_string()),
//! #    protocol: ConnectionProtocol::TcpUdp,
//! #    discovery: DiscoveryMethods::Relay,
//! #    credentials: Credentials {
//! #        username: "username".to_string(),
//! #        password: Some("password".to_string()),
//! #    },
//! #    debug: false,
//! #    max_discovery_retries: 10,
//! # };
//! # let mut camera = BcCamera::new(&options).await.unwrap();
//! camera.login().await;
//! # })
//! ```
//! For further commands see the [`bc_protocol::BcCamera`] struct.
//!

/// Contains low level BC structures and formats
pub mod bc;
/// Contains high level interfaces for the camera
pub mod bc_protocol;
/// Contains low level structures and formats for the media substream
pub mod bcmedia;
///  Contains low level structures and formats for the udpstream
pub mod bcudp;

/// This is the top level error structure of the library
///
/// Most commands will either return their `Ok(result)` or this `Err(Error)`
pub use bc_protocol::Error;

pub(crate) use bc_protocol::{Credentials, Result};

pub(crate) type NomErrorType<'a> = nom_language::error::VerboseError<&'a [u8]>;

#[cfg(test)]
pub(crate) fn assert_fragmented_parse<T>(
    sample: &[u8],
    mut deserialize: impl FnMut(&mut bytes::BytesMut) -> Result<T>,
) {
    for split in 0..sample.len() {
        let mut buf = bytes::BytesMut::from(&sample[..split]);
        assert!(
            matches!(deserialize(&mut buf), Err(Error::NomIncomplete(_))),
            "Incomplete packet at split {} must request more data", split,
        );
        assert_eq!(
            &buf[..], &sample[..split], "Incomplete input must stay buffered",
        );
        buf.extend_from_slice(&sample[split..]);
        assert!(
            deserialize(&mut buf).is_ok(),
            "Complete packet must parse after split {}", split,
        );
        assert!(buf.is_empty(), "Complete packet must be consumed exactly once");
    }
}
