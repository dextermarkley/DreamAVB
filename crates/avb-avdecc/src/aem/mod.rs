// SPDX-License-Identifier: MIT OR Apache-2.0

//! The AEM model, descriptor codecs and dump (design §9.2, §9.3).

pub mod desc;
pub mod json;
pub mod model;

pub use model::{
    AudioClusterDesc, AudioMapDesc, AudioMapping, AudioUnitDesc, AvbInterfaceDesc, ClockDomainDesc,
    ClockSourceDesc, ConfigurationDesc, Descriptor, EntityDesc, StreamDesc, StreamPortDesc,
    TalkerAem,
};
