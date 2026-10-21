# Changelog

## v0.10.1 - 2026-10-21

This release fixes a security advisory in a certificate library and restores the CI build.

### Fixes

- The v0.10.0 release built no artifacts because CI failed. This fix updates `rustls-webpki` to 0.103.16 (RUSTSEC-2026-0382) and makes CI pass.

## v0.10.0 - 2026-10-20

This release lets you run Thurm with a headscale coordination server. It also lets you pair a phone when HTTPS is off on your tailnet.

### New features

- Set `tailscale.control_url` to sign in to headscale or another coordination server. Set `tailscale.plain_listen` to pair a phone when HTTPS is off. The Pair iPhone sheet now checks the command-line tool and shows how to install it. A new Linux server guide and `install.sh` script install Thurm on Linux.
