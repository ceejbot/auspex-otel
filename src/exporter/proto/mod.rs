//! Vendored OTLP protobuf types (narrow trace subset).
//!
//! The per-package `.rs` files in this directory are **generated** by
//! `prost-build` from the vendored `.proto` files under `proto/`. Do not edit
//! them by hand — regenerate with `just gen-proto` (see `proto/README.md`).
//!
//! The module tree below mirrors the proto package hierarchy
//! (`opentelemetry.proto.<signal>.v1`) so that prost's cross-package
//! `super::...` references resolve. The `include!`d files live alongside this
//! one.
//!
//! Generated code does not follow this crate's strict lint baseline, so the
//! inner attributes below relax it for this module only.
#![allow(
    clippy::all, clippy::pedantic, clippy::nursery, clippy::unwrap_used, clippy::doc_markdown, rust_2018_idioms,
    trivial_casts, trivial_numeric_casts, unused_qualifications, unused_lifetimes, missing_docs,
    dead_code // generated: not every OTLP message type is referenced
)]

pub mod opentelemetry {
    pub mod proto {
        pub mod common {
            pub mod v1 {
                include!("opentelemetry.proto.common.v1.rs");
            }
        }
        pub mod resource {
            pub mod v1 {
                include!("opentelemetry.proto.resource.v1.rs");
            }
        }
        pub mod trace {
            pub mod v1 {
                include!("opentelemetry.proto.trace.v1.rs");
            }
        }
        pub mod collector {
            pub mod trace {
                pub mod v1 {
                    include!("opentelemetry.proto.collector.trace.v1.rs");
                }
            }
        }
    }
}

// Convenience re-exports mirroring the paths the exporter uses, so call sites
// can write `proto::trace_v1::Span` etc. instead of the full package path.
pub use opentelemetry::proto::collector::trace::v1 as collector_trace_v1;
pub use opentelemetry::proto::common::v1 as common_v1;
pub use opentelemetry::proto::resource::v1 as resource_v1;
pub use opentelemetry::proto::trace::v1 as trace_v1;
