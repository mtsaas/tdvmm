# Reject fixture: an UNPINNED base image. build: is supported in 2b, but every
# external FROM must be digest-pinned (@sha256:) for a reproducible closed-world
# build. This one is a floating tag, so bake_compose.py must reject it loudly.
FROM docker.io/library/alpine:3.22
RUN echo hi
