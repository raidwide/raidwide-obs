# Raidwide OBS metadata

Metadata is inserted before the first coded frame, without transcoding:

- **H.264:** unregistered user-data SEI, NAL type 6 (Annex B).
- **HEVC:** unregistered user-data prefix SEI, NAL type 39 (Annex B).
- **AV1:** private metadata OBU, type 5 / metadata type 6 (low-overhead framing).

Payloads start with UUID `7261696477696465ff14ff14ff14ff14`;
AV1 adds version byte `1` before the application data.
