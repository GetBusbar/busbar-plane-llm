// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Historical dialect expectations used by the LLM owner's compatibility tests.
//! Runtime credentials are handled through the auth-kind contract; these values
//! do not register an auth implementation or grant access to credential material.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IngressAuth {
    Bearer,
    SigV4,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CredentialHeader {
    Bearer,
    Raw {
        header: &'static str,
        trim_start: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CredentialFamily {
    pub prefix: &'static str,
    pub presented_as: CredentialHeader,
}

#[derive(Clone, Copy, Debug)]
pub enum EgressScheme {
    Static {
        families: &'static [CredentialFamily],
        own: CredentialHeader,
        passthrough: CredentialHeader,
    },
    SigV4 {
        service: &'static str,
        region_of_host: fn(&str) -> Option<&str>,
        default_region: &'static str,
        content_type: &'static str,
    },
}

impl EgressScheme {
    pub const fn bearer() -> Self {
        Self::Static {
            families: &[],
            own: CredentialHeader::Bearer,
            passthrough: CredentialHeader::Bearer,
        }
    }

    pub const fn header(header: &'static str) -> Self {
        let raw = CredentialHeader::Raw {
            header,
            trim_start: false,
        };
        Self::Static {
            families: &[],
            own: raw,
            passthrough: raw,
        }
    }
}
