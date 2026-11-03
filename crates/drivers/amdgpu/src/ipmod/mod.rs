//! GX-6m: one module per IP block and version, chosen from the discovery table (GX-D2, Georg 24.10.
//! "Baukastenartig"). Each module file names the versions it is written for, the Linux block that serves the same
//! versions (the reference), the firmware it needs and where its code stands. `plan` walks the roles a GPU needs:
//! a role whose IP the table reports gets the module that covers that exact version, or is reported **missing**
//! (with Linux's block as the candidate if one module's Linux block serves the version) - never a guess from a
//! neighbouring version. Nothing here is keyed to a PCI ID: the 780M is "these versions", an RX 9070 is others.
//!
//! The roles key on the IPs Linux's `amdgpu_discovery_set_ip_blocks` keys on (linux-6.6 amdgpu_discovery.c:1600-2135):
//! GFX, MES, GMC and GFXHUB on GC; SDMA on SDMA0; PSP on MP0; SMU on MP1; display on DMU; VCN on UVD/VCN; IH on
//! OSSSYS; MMHUB on MMHUB; NBIO on NBIF.

use crate::gpucore::disc::Discovery;
use crate::gpucore::fw::{self, Need};
use crate::gpucore::hwid;
use crate::ip::IpVersion;
use alloc::string::String;
use alloc::vec::Vec;

pub mod dcn31;
pub mod gfx11;
pub mod gmc11;
pub mod ih6;
pub mod mes11;
pub mod mesapi;
pub mod mesmqd;
pub mod nbio7;
pub mod psp13;
pub mod sdma6;
pub mod smu13;
pub mod vcn4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Gfx,
    Mes,
    Sdma,
    Psp,
    Smu,
    Display,
    Vcn,
    Ih,
    Gmc,
    Gfxhub,
    Mmhub,
    Nbio,
}

impl Role {
    pub const ALL: [Role; 12] = [
        Role::Psp,
        Role::Smu,
        Role::Nbio,
        Role::Gmc,
        Role::Gfxhub,
        Role::Mmhub,
        Role::Ih,
        Role::Gfx,
        Role::Mes,
        Role::Sdma,
        Role::Display,
        Role::Vcn,
    ];

    /// The hardware ID whose instance-0 version picks the module.
    pub const fn key(self) -> u16 {
        match self {
            Role::Gfx | Role::Mes | Role::Gmc | Role::Gfxhub => hwid::GC,
            Role::Sdma => hwid::SDMA0,
            Role::Psp => hwid::MP0,
            Role::Smu => hwid::MP1,
            Role::Display => hwid::DMU,
            Role::Vcn => hwid::VCN,
            Role::Ih => hwid::OSSSYS,
            Role::Mmhub => hwid::MMHUB,
            Role::Nbio => hwid::NBIF,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Role::Gfx => "gfx",
            Role::Mes => "mes",
            Role::Sdma => "sdma",
            Role::Psp => "psp",
            Role::Smu => "smu",
            Role::Display => "display",
            Role::Vcn => "vcn",
            Role::Ih => "ih",
            Role::Gmc => "gmc",
            Role::Gfxhub => "gfxhub",
            Role::Mmhub => "mmhub",
            Role::Nbio => "nbio",
        }
    }
}

/// Where a module's code stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Working code, at this path.
    Present(&'static str),
    /// Identity, versions, firmware and the register facts the probe uses are here; the bring-up comes with
    /// this item.
    Planned(&'static str),
}

#[derive(Debug)]
pub struct Module {
    pub name: &'static str,
    pub role: Role,
    /// The versions this module is written and checked for: the only ones `plan` chooses it for.
    pub versions: &'static [IpVersion],
    /// The Linux block serving these versions (the reference for behaviour).
    pub linux: &'static str,
    /// Further versions Linux serves with the same block: reported as a candidate when missing, never chosen.
    pub linux_also: &'static [IpVersion],
    /// Firmware name prefix (`gc`, `psp`, ...) and the files, or `None` with the reason there is none.
    pub firmware: Firmware,
    pub state: State,
}

#[derive(Clone, Copy, Debug)]
pub enum Firmware {
    Files { prefix: &'static str, needs: &'static [Need] },
    None(&'static str),
}

pub const fn v(major: u8, minor: u8, rev: u8) -> IpVersion {
    IpVersion { major, minor, rev }
}

/// Every module BitsOS has (one entry per module file).
pub static MODULES: &[&Module] = &[
    &psp13::MODULE,
    &smu13::MODULE,
    &nbio7::MODULE,
    &gmc11::GMC,
    &gmc11::GFXHUB,
    &gmc11::MMHUB,
    &ih6::MODULE,
    &gfx11::MODULE,
    &mes11::MODULE,
    &sdma6::MODULE,
    &dcn31::MODULE,
    &vcn4::MODULE,
];

/// What `plan` decided for one role.
#[derive(Clone, Copy, Debug)]
pub enum Choice {
    Chosen { role: Role, version: IpVersion, module: &'static Module },
    /// The GPU has the IP in this version and no module covers it. `candidate`: a module whose Linux block
    /// serves the version (a starting point, not a choice).
    Missing { role: Role, version: IpVersion, candidate: Option<&'static Module> },
    /// The IP is fused off (harvest table).
    Harvested { role: Role, version: IpVersion },
    /// The table has no such IP (a headless GPU has no DMU): nothing is needed.
    Absent { role: Role },
}

impl Choice {
    pub fn role(&self) -> Role {
        match *self {
            Choice::Chosen { role, .. }
            | Choice::Missing { role, .. }
            | Choice::Harvested { role, .. }
            | Choice::Absent { role } => role,
        }
    }
}

/// One firmware file a chosen module needs (GX-7a loader input).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileNeed {
    pub role: Role,
    pub suffix: &'static str,
    pub name: String,
    pub fallback: Option<String>,
}

#[derive(Debug)]
pub struct Plan {
    pub choices: Vec<Choice>,
}

/// Chooses a module for every role from the discovery table.
pub fn plan(d: &Discovery) -> Plan {
    plan_with(d, MODULES)
}

pub fn plan_with(d: &Discovery, modules: &[&'static Module]) -> Plan {
    let choices = Role::ALL
        .iter()
        .map(|&role| {
            let Some(e) = d.find(role.key(), 0) else {
                return Choice::Absent { role };
            };
            let version = e.version;
            if d.harvested(e) {
                return Choice::Harvested { role, version };
            }
            let of_role = || modules.iter().copied().filter(move |m| m.role == role);
            match of_role().find(|m| m.versions.contains(&version)) {
                Some(module) => Choice::Chosen { role, version, module },
                None => Choice::Missing { role, version, candidate: of_role().find(|m| m.linux_also.contains(&version)) },
            }
        })
        .collect();
    Plan { choices }
}

impl Plan {
    pub fn missing(&self) -> impl Iterator<Item = &Choice> {
        self.choices.iter().filter(|c| matches!(c, Choice::Missing { .. }))
    }

    pub fn chosen(&self, role: Role) -> Option<&'static Module> {
        self.choices.iter().find_map(|c| match *c {
            Choice::Chosen { role: r, module, .. } if r == role => Some(module),
            _ => None,
        })
    }

    /// GX-7a: the same files as `firmware`, structured for the loader (`gpucore::fwload`): role, suffix, name,
    /// fallback name.
    pub fn files(&self) -> Vec<FileNeed> {
        let mut out: Vec<FileNeed> = Vec::new();
        for c in &self.choices {
            let Choice::Chosen { role, version, module } = *c else { continue };
            let Firmware::Files { prefix, needs } = module.firmware else { continue };
            for n in needs {
                let name = fw::file(prefix, version, n.suffix);
                if !out.iter().any(|f| f.name == name) {
                    out.push(FileNeed { role, suffix: n.suffix, name, fallback: n.fallback.map(|f| fw::file(prefix, version, f)) });
                }
            }
        }
        out
    }

    /// The firmware files the chosen modules need, each with its fallback (`a.bin|b.bin`), in role order, each
    /// file once (GFX and MES share the `gc` prefix).
    pub fn firmware(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in &self.choices {
            let Choice::Chosen { version, module, .. } = *c else { continue };
            let Firmware::Files { prefix, needs } = module.firmware else { continue };
            for n in needs {
                let mut s = fw::file(prefix, version, n.suffix);
                if let Some(f) = n.fallback {
                    s.push('|');
                    s += &fw::file(prefix, version, f);
                }
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
        out
    }
}
