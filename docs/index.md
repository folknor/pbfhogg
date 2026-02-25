---
layout: home

hero:
  name: "Kvakk"
  text: "Quick Share for Linux and Windows"
  tagline: "Send and receive files with Android devices on your local network using Google's Quick Share protocol. No phone app, no cloud, no fuss."
  image:
    src: /kvakk-logo.svg
    alt: Kvakk logo
  actions:
    - theme: brand
      text: Get Started
      link: /guide/
    - theme: alt
      text: GitHub
      link: https://github.com/user/kvakk

features:
  - icon:
      src: /icons/globe.svg
    title: Local Network Discovery
    details: Discovers nearby Android devices via mDNS. No pairing, no cloud relay — files move directly over your local network.
  - icon:
      src: /icons/gauge.svg
    title: Full Quick Share Protocol
    details: Implements the complete Google Quick Share handshake with P-256 ECDH, AES encryption, and proper acknowledgment.
  - icon:
      src: /icons/wrench.svg
    title: Single Binary
    details: Built in Rust with egui. One binary, no dependencies, no runtime. Works on Linux and Windows.
---

## Why Kvakk?

Google's Quick Share (formerly Nearby Share) lets Android devices share files seamlessly — but the official desktop client is Chrome-only and Windows-only. Kvakk brings native Quick Share support to Linux and Windows as a lightweight Rust binary with a minimal GUI.

### How It Works

1. **Discover** — mDNS broadcasts find nearby Android devices on the local network
2. **Connect** — A secure TCP connection is established with P-256 ECDH key exchange
3. **Transfer** — Files are encrypted with AES and streamed directly between devices
4. **Confirm** — PIN-based verification ensures you're sending to the right device
