# Security Policy

## Reporting a vulnerability

Please report security problems privately via
[GitHub security advisories](https://github.com/TillBeemelmanns/shw/security/advisories/new)
rather than in a public issue. You can expect an initial answer within a week.

## Threat model

shw is meant to be safe to point at **untrusted files**: a Markdown document, image or video from
a cloned repository or the internet. In particular:

- **No terminal injection.** Control characters in documents, source files, file names and link URLs
  are replaced before output, so a file cannot emit escape sequences (e.g. OSC 52 clipboard writes).
- **No code execution from documents.** shw runs no scripts and no browser. External tools
  (`ffmpeg`, `ffprobe`, `ffplay`, `mmdc`) are only used if already installed, are resolved via
  absolute `PATH` entries only (a checkout cannot shadow them with `./ffmpeg`), and `mmdc` runs in a
  private temporary directory so project config files are not loaded. Nothing is installed on demand.
- **Safe links.** Only `http`, `https` and `mailto` links and local documents/images are clickable;
  other schemes and links to scripts or applications are shown as plain text.
- **Remote images.** Images referenced by Markdown are downloaded like on github.com, but only from
  public internet hosts: loopback, private, link-local and similar addresses are refused, including
  after redirects. Use `--offline` (or `SHW_OFFLINE=1`) to disable all downloads.
- **Temporary files** are created with unpredictable names, `O_EXCL` and mode `0600`.

## Verifying releases

Release archives are built by GitHub Actions from the tagged commit, with every action pinned to a
commit SHA. Each archive has a `.sha256` checksum (checked by `install.sh`) and a signed build
provenance attestation:

```sh
gh attestation verify shw-v*-*.tar.gz --repo TillBeemelmanns/shw
```
