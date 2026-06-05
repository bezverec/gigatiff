# IIIF Image API 3.0 Compliance

This document tracks GigaTIFF's current IIIF Image API 3.0 coverage.

Reference specifications:

- IIIF Image API 3.0: https://iiif.io/api/image/3.0/
- IIIF Image API 3.0 Compliance: https://iiif.io/api/image/3.0/compliance/

## Current Target

GigaTIFF currently targets Image API 3.0 `level2` for JPEG, PNG, and WebP responses.
The server advertises only optional features it implements.

## Compliance Matrix

| Area | Feature | Status | Notes |
| --- | --- | --- | --- |
| Information | `info.json` | Supported | Returns `ImageService3`, JSON-LD content type, dimensions, preferred sizes, tiles, `maxArea`, preferred/extra formats, extra qualities, and extra features. |
| Information | `profile: level2` | Supported | Advertised when the server is built with the current server implementation. |
| Information | `tiles` | Supported | Advertises square tiles with powers-of-two scale factors. |
| Information | `sizes` | Supported | Advertises powers-of-two full-image variants that stay within `maxArea`. |
| Region | `full` | Supported | Required at all levels. |
| Region | `x,y,w,h` | Supported | Crops to image edge when the region extends beyond bounds. Empty/out-of-bounds regions return `400`. |
| Region | `pct:x,y,w,h` | Supported | Converted to pixel coordinates and clipped consistently with pixel regions. |
| Region | `square` | Supported | Uses a centered square crop. |
| Size | `max` | Supported | Respects `maxArea`; may be smaller than full dimensions for very large regions. |
| Size | `w,` | Supported | Aspect-preserving width request. |
| Size | `,h` | Supported | Aspect-preserving height request. |
| Size | `pct:n` | Supported | Percentage resize. |
| Size | `!w,h` | Supported | Aspect-preserving fit within a box. |
| Size | `w,h` | Supported | Exact width/height request, including distortion. |
| Size | `^size` | Supported | Explicit upscaling for supported size forms. Non-caret upscaling returns `400`. |
| Rotation | `0` | Supported | Required at all levels. |
| Rotation | `90`, `180`, `270` | Supported | Advertised as `rotationBy90s`. |
| Rotation | arbitrary values | Not advertised | Requests such as `45` return `400`. |
| Rotation | mirroring `!n` | Supported | Mirroring is applied before rotation, as specified. |
| Quality | `default` | Supported | Same output as `color`. |
| Quality | `color` | Supported | Color response; advertised in `extraQualities`. |
| Quality | `gray` | Supported | Converts RGB channels to luminance while preserving alpha; advertised in `extraQualities`. |
| Quality | `bitonal` | Supported | Applies a simple luminance threshold; advertised in `extraQualities`. |
| Format | `jpg` / `jpeg` | Supported | JPEG output; canonical links use `jpg`. |
| Format | `png` | Supported | PNG output. |
| Format | `webp` | Supported | WebP output; optional and advertised in `extraFormats`. |
| Format | `tif`, `gif`, `pdf`, `jp2` | Not advertised | Optional formats are intentionally unsupported for now. |
| HTTP | CORS | Supported | `CorsLayer::permissive()` is enabled. |
| HTTP | JSON-LD media type | Supported | `info.json` uses `application/ld+json` with the IIIF profile parameter. |
| HTTP | Base URI redirect | Supported | `/iiif/3/{identifier}` returns `303` to `/info.json`. |
| HTTP | Profile Link header | Supported | `Link: <http://iiif.io/api/image/3/level2.json>;rel="profile"`. |
| HTTP | Canonical Link header | Supported | Image responses include a canonical image URI. |
| Cache | Canonical response cache key | Supported | Equivalent image requests are cached by canonical IIIF image URI, not raw request spelling. |

## Smoke Test

With the compose stack running:

```powershell
docker compose up -d --build
.\scripts\iiif-smoke.ps1 -BaseUrl http://127.0.0.1:18082 -ImageId mapa2.tif
```

The smoke test checks the advertised profile, HTTP features, preferred-size requests, advertised tile geometry, representative Level 2 image requests, selected negative requests, and canonical cache-key reuse for equivalent requests.

The GitHub Actions `IIIF server smoke` job generates a tiny TIFF fixture, starts the Docker Compose stack, and runs this smoke test on every push and pull request.
