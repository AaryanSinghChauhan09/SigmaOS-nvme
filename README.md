# SigmaOS-nvme

Component boundary for NVMe controller access, queue pairs, DMA-safe
submission, completion handling, and device error recovery. Policy and
user-facing storage management belong in `SigmaOS-services`.

## Current state

The component manifest marks this repository as a scaffold. The current
checkout contains metadata, a README, a license, and a metadata validation
workflow; it does not implement an NVMe controller or expose a driver API.

## Intended boundary

The future driver contract must define controller discovery, MMIO ownership,
queue lifecycle, DMA buffer constraints, interrupt or polling completion,
timeouts, reset behavior, and error propagation. Storage policy should consume
that contract without owning controller details.

## Readiness evidence

Do not claim hardware support until the implementation documents its device
scope and memory-safety assumptions, has reproducible queue and error-path
checks, and has evidence from supported hardware or a clearly identified
simulator. The current validation workflow checks metadata only.
