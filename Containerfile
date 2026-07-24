# Minimal OCI image distributing the bootc-migrate binary.
#
# This tool needs to run directly on the host (root, /sysroot, EFI vars) — it
# is not meant to run *inside* this container. The image exists so the binary
# can be pulled without hitting GitHub Release rate limits/auth, or referenced
# from another Containerfile:
#
#   podman create --name bmc-extract ghcr.io/tuna-os/bootc-migrate:latest
#   podman cp bmc-extract:/usr/local/bin/bootc-migrate .
#   podman rm bmc-extract
#
#   # or, in another image's build:
#   COPY --from=ghcr.io/tuna-os/bootc-migrate:latest \
#       /usr/local/bin/bootc-migrate /usr/local/bin/
#
# Built by CI (.github/workflows/release.yml) from the per-arch release
# binaries — see ctx/linux/$TARGETARCH/ staged there before this build.
FROM gcr.io/distroless/cc-debian12
ARG TARGETARCH
COPY linux/${TARGETARCH}/bootc-migrate /usr/local/bin/bootc-migrate
ENTRYPOINT ["/usr/local/bin/bootc-migrate"]
