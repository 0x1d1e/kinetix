# Security Policy

Kinetix is an Internet-facing proxy that handles sensitive LLM credentials, virtual authentication keys, and real-time streaming traffic. Security and confidentiality are central to its design.

## Supported Versions

Only the latest release line receives active security updates and vulnerability patches:

| Version | Supported          |
| ------- | ------------------ |
| 0.2.x   | :white_check_mark: |
| < 0.2.0 | :x:                |

## Reporting a Vulnerability

We take the security of Kinetix seriously. If you discover a vulnerability, please report it responsibly:

1. **GitHub Security Advisory (Preferred)**:
   Submit a private report via [GitHub Private Vulnerability Reporting](https://github.com/0x1d1e/kinetix/security/advisories/new).
2. **Alternative Disclosure**:
   If GitHub Security Advisories are inaccessible, open a confidential issue or contact the maintainer directly through the contact details listed on [GitHub profile](https://github.com/0x1d1e).

Please **do not** report suspected security vulnerabilities through public GitHub issues or discussions.

### What to Include in Your Report

To help us investigate and resolve the issue quickly, include:
- A detailed description of the vulnerability and its potential impact.
- Step-by-step reproduction instructions or a minimal Proof of Concept (PoC).
- The version or commit hash where the vulnerability was observed.
- The operating environment (OS, architecture, deployment topology).

### Our Response Timeline

- **Acknowledgement**: Within 48 hours of initial report receipt.
- **Triage & Assessment**: Within 5 business days with confirmation of validity and severity.
- **Fix & Disclosure**: Coordinated patch release with credit to the reporter (unless anonymity is requested).

## Security Architecture & Threat Model

Kinetix implements deliberate security boundaries:

- **Credential Isolation**: Upstream provider API keys and tokens are stored encrypted at rest using AES-256-GCM keyed by the master key (`KINETIX_MASTER_KEY`). Client virtual keys cannot read or extract upstream credentials.
- **Topology Hiding**: Internal upstream endpoints, provider names, and routing accounts are never exposed to clients. Responses include opaque, non-reversible route identifiers (`X-Kinetix-Route-Id`) that only administrators can resolve via authenticated admin APIs.
- **Control Plane Hardening**: The admin API and management dashboard require separate authentication (`KINETIX_ADMIN_TOKEN` or Cloudflare Access headers) and support dedicated host/port isolation.
- **Supply Chain Integrity**: Dependency licenses, known advisories, and banned crates are verified on every commit via `cargo-deny`. Releases provide cryptographic SHA256 checksums and GitHub build provenance attestations.
