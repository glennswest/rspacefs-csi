//! Generated CSI v1 gRPC bindings.
//!
//! The Container Storage Interface protobuf (`proto/csi.proto`, spec v1.9.0) is
//! compiled at build time by `tonic-build`. Everything below is re-exported from
//! the generated `csi.v1` package so the driver can `use csi_proto::*`.

#![allow(clippy::all)]

tonic::include_proto!("csi.v1");
