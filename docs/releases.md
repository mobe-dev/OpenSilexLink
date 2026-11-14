# Publishing releases

This fork is maintained at https://github.com/mobe-dev/OpenSilexLink; the original
project is https://github.com/bytewarrior/OpenSilexLink. Preserve the original
license and history when contributing or publishing another fork.

## Build locally

Install Rust **1.93.1**, Python with pytest/PySide6, and nfpm **2.45.0**.
Run:

```sh
packaging/build-packages.sh 1.1.0 dist/release
(cd dist/release && sha256sum -c SHA256SUMS)
```

The build runs Rust and Python tests, checks all three application versions,
collects dependency notices, and creates the application `.deb` and `.rpm`.
It does not install packages or restart the running service. Packages built on
the Ubuntu 24.04 CI runner require glibc 2.39 or newer. Native builds on newer
hosts may require a newer glibc; use CI artifacts for the published release.

Review [licensing](licensing.md) before changing the asset list. Do not publish
the optional repacked EVDI binary, vendor firmware, modified root filesystems,
private recovery backups, keys, captures or machine configuration. Do not
upload the entire local `dist/` directory.

## Publish on GitHub

The Build workflow runs tests, builds packages and installs them in Debian,
Ubuntu and Fedora containers. A version tag invokes those same checks; the
Release workflow only publishes after every install test passes.

1. Update `daemon/Cargo.toml`, `daemon/Cargo.lock`, `client/pyproject.toml`,
   `client/src/opensilexlink/__init__.py` and `CHANGELOG.md` together.
2. Commit and push `main` to the fork. Enable Actions on a new fork if GitHub
   has disabled them.
3. Create and push the matching tag:

   ```sh
   git tag -a v1.1.0 -m "OpenSilexLink 1.1.0"
   git push origin main
   git push origin v1.1.0
   ```

4. Wait for the Release workflow. It publishes four explicit assets:
   `opensilexlink_1.1.0_amd64.deb`,
   `opensilexlink-1.1.0-1.x86_64.rpm`, `THIRD_PARTY_NOTICES.txt`, and `SHA256SUMS`.
   Checksums detect corruption; they are not independent signatures.

Packages are GitHub Release downloads. This project does not operate an APT
or RPM repository, and these files are not container images in GHCR.
