---
tags: [meta, agents, governance, linux, hardware, embedded]
---

# AGENTS.md — KURURU-TAB3LITE-LINUX Governance Rules

This repository contains **KURURU-TAB3LITE-LINUX**, the bare-metal headless Linux transform and hardware daemons for Samsung Galaxy Tab 3 Lite 7.0 (`SM-T110`, codename `goyawifi`), turning legacy Android tablets into low-power Linux nodes for the Mnemocine Homelab.

When modifying any file in this repository, follow these mandatory governance rules:

**Language Tier:** A (Public OSS) — see [language-policy.md](../../agentic-ai/governance/language-policy.md). All logs, CLI strings, documentation, and comments MUST be in English.

## 🦀 Standards & Integrity

1. **RUST MIGRATION DIRECTIVE** — All hardware daemons, userland tools, and background utilities must be implemented in native Rust (`edition = "2021"` or `"2024"`). Do not introduce Python or runtime dependencies on the target (`ARCH-NO-PYTHON`).

2. **MANDATORY STENIOSENTINEL VERIFICATION (RULE 0)** — Before completing any turn or committing, execute `stenio --path .` or `stenio --diff`. The Quality Gate must pass with zero blocking errors.

3. **SECURITY & ZERO SECRETS (`SEC-SECRETS`)** — Never commit Wi-Fi passwords, Tailscale auth keys, or private SSH keys. Use `.example` files for configuration templates.

4. **STANDARDIZED README DISCLAIMER** — The root `README.md` must preserve the standardized governance disclaimer:
   ```markdown
   <div align="center">

   ### 🛡️ Human-in-the-Loop Agentic Engineering & Deterministic Governance

   > **Architected by an Anthropologist, Built with Autonomous AI Agents, Governed by Deterministic Code.**
   > 
   > This project was developed through rigorous human-AI pair programming led by **Carlos Eduardo Rodrigues** ([@ceduardorodrig](https://github.com/ceduardorodrig)) — an anthropologist and product architect using autonomous coding agents under strict, sub-millisecond static governance.
   >
   > Every commit, driver, and system architecture is continuously audited and enforced by 🤖 **[StenioSentinel](https://github.com/ceduardorodrig/STENIO-SENTINEL)** (our native Rust quality gate) with zero tolerance for hallucinated tests, blind merges, or bypassed checks.

   </div>
   ```

5. **DRY PRINCIPLE (`ARCH-DRY-DUPLICATION`)** — Never duplicate substantive logic. Extract shared code into helpers (e.g. the data-driven framebuffer sprite blitter and the shared input-device listener). Run `stenio --dry --path .` and require **zero** duplicated blocks before every commit.
