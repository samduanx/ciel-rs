//! Thin wrapper around the upstream systemd-nspawn machinery (`machine.rs`).
//!
//! The upstream logic is deliberately untouched — this file only adapts the
//! signatures to the `ContainerBackend` trait so the nspawn path stays
//! byte-identical to upstream (fork merge-ability).

use anyhow::Result;
use std::ffi::OsStr;

use super::{BackendKind, ContainerBackend, CielInstance, InstanceHandle};
use crate::machine;

pub struct NspawnBackend;

impl ContainerBackend for NspawnBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Nspawn
    }

    fn execute(&self, inst: &InstanceHandle, args: &[&OsStr]) -> Result<i32> {
        machine::execute_container_command(&inst.ns_name, args)
    }

    fn stop(&self, inst: &InstanceHandle) -> Result<()> {
        machine::terminate_container_by_name(&inst.ns_name)
    }

    fn inspect(&self, inst: &InstanceHandle) -> Result<CielInstance> {
        machine::inspect_instance(&inst.name, &inst.ns_name)
    }

    fn clean_children(&self) {
        machine::clean_child_process()
    }
}
