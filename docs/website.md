# meshguard.net

The public website. Plain static HTML, CSS and a little JavaScript in `website/`, with no build step. A push to `main` that touches `website/**` deploys it to Vercel (`.github/workflows/deploy-website.yml`). `vercel.json` turns on clean URLs, so `getting-started.html` is served at `/getting-started`, and `404.html` is the not-found page.

## Pages

| Page | File | Purpose |
|------|------|---------|
| Home | `index.html` | What MeshGuard is, the short answers, how it works, four steps, features, roadmap teaser |
| How It Works | `how-it-works.html` | Message path, connections, the two encryption layers, delivery ticks, range |
| Getting Started | `getting-started.html` | The flagship guide: two Android phones, two SenseCAP T1000-E radios |
| Features | `features.html` | Every feature with its status and the release it arrived in |
| Supported hardware | `hardware.html` | Tested vs compatible-but-not-verified radios, requirements |
| Download | `download.html` | Release downloads, checksums, changelog, build from source |
| Tutorial videos | `videos.html` | Four lesson slots, not published yet |
| FAQ | `faq.html` | Questions and troubleshooting |
| Roadmap | `roadmap.html` | Shipped, in development, planned, not planned |
| Privacy | `privacy.html` | Privacy policy |

The header and footer are repeated in every page. When you change the navigation or the footer, change it in all of them.

## Design

- Light by default. Dark is opt-in: the moon button saves `meshguard-theme = dark` in local storage. `js/theme.js` runs in `<head>` before the page paints, so there is no flash of the wrong theme.
- Colours are tokens at the top of `css/site.css` (`--color-bg`, `--color-surface`, `--color-primary`, `--color-text`, `--color-border`, `--color-muted`, and a few more). Dark mode only redefines the tokens under `:root[data-theme="dark"]`.
- Text contrast meets WCAG AA in both themes. `--color-primary-on-soft` exists because the brand green is just under 4.5:1 on the soft green background.
- Fonts: DM Sans for text, JetBrains Mono for codes, commands and node IDs (Google Fonts, as noted in the privacy policy).
- No tracking, no third-party scripts. The pages work without JavaScript; the menu then wraps under the logo.
- Status badges always carry text (Available, New, In development, Planned), never colour alone.
- `css/` and `js/` are cached for a year. Bump the `?v=` query in every page when you change them.

## Keeping the copy true

Features and the roadmap must match the release people can download, not the main branch.

On every release:
1. Update the **Since** column and the **New** badges in `features.html`, and move items on `roadmap.html`.
2. Check the getting-started steps against the app: button names, screens, permissions.
3. Keep the rules from the redesign brief: no fixed range promise, SOS is never presented as an emergency service, MeshGuard encryption is described separately from Meshtastic channel encryption, and in-development features are labelled as such until they are tested on real radios.

The Android **IzzyOnDroid** listing is not live yet, so the site does not link to it. Add the button on `download.html` when the listing exists.

## Guide images

The guide uses generated app illustrations from the updated media starter pack (`media/guide/*.webp`, 640 px wide, about 40 KB each). They are **not captures of the release**: the UI, labels and some values differ from the real app. Each one is captioned with an *Illustration* tag and "Not a screenshot of the app". Where the picture contradicts the app, the caption gives the correct fact. The guide text is the source of truth.

| Image | Step | Known differences, corrected in the caption |
|-------|------|------|
| `01-radio-region-preset-channel` | A | Shows radio settings inside MeshGuard; they are set in the Meshtastic app |
| `02-ble-scan` | B | Says default PIN 000000 and "MeshGuard firmware"; the real default is 123456 and the radios run Meshtastic |
| `04-contact-invite-qr` | C | Demo QR code, does not work |
| `05-scan-contact-qr` | C | None noted |
| `07-message-delivery-ack` | D | Uses "Delivered"/"ACK" labels; the app shows ✓ and ✓✓ |
| `08-check-in` | E | A separate check-in screen with "Check location"; the real buttons sit above the message box in a chat |
| `09-radio-diagnostics` | E | Says "2.4 GHz LoRa" (the T1000-E uses sub-GHz LoRa, 868 or 915 MHz). Shows battery time left, firmware version and GPS fix, which the real diagnostics sheet does not have |
| `10-sos-warning` | E | Shows a "Share my location" switch; the real SOS is a plain confirmation and always tries to include the location |

Still placeholders (`.shot` figures), with no image yet:

| Placeholder | What it should show |
|-------------|---------------------|
| `getting-started/b-connected.png` | Mesh Network screen with the radio connected |
| `getting-started/c-paired.png` | Chats list with the new contact, Encrypted Chat |

When real captures from the current release exist, replace the illustrations with them, hiding channel keys and pairing codes, and drop the *Illustration* tag. Do not put the illustrations anywhere they could be read as real captures, such as the home page, store listings or social posts.

The SVG feature cards in the starter pack are not used: they bake text into the image.

## Animations

Two short HeyGen clips sit in `media/`. Both are decorative, never shown as app behaviour, and captioned as illustrations.

| File | Where | What it shows |
|------|-------|---------------|
| `media/hero.mp4`, `media/hero-poster.jpg` | Home hero | A phone and a radio on a ridge, linked to a second pair |
| `media/how.mp4`, `media/how-poster.jpg` | How It Works, beside the message path | A message hopping between radios over terrain |

- Re-encoded from the HeyGen downloads: audio removed, 1280 px wide, H.264, `+faststart` (about 1.8 MB and 0.4 MB). Keep new clips that small.
- Copy the `<figure class="media-video">` markup from `index.html`: `muted loop playsinline preload="none"`, `aria-hidden` on the `<video>`, a poster, a caption, and the pause button.
- `js/site.js` plays a clip only while it is on screen. It stays paused (poster only) for visitors with reduced motion or Data Saver on, and without JavaScript. The button pauses or plays it (WCAG 2.2.2).

## Videos

`videos.html` has four lesson slots that say "Video in production". They link to the matching written guide. When a video is ready:
- Embed it with captions (a `<track>` file or the host's captions) and publish a transcript on the page.
- Show real screen recordings of the current release; any presenter or AI voice stays secondary.
- Never add a player or a URL before the video exists.

## Checks before merging

- Every page at 320, 375, 768, 1024 and 1440 px wide: no horizontal scrolling.
- Light on a fresh visit; the toggle switches and persists across pages.
- Keyboard: the skip link comes first, focus is visible, the menu opens with Enter and closes with Escape.
- One `h1` per page, no skipped heading levels.
- All outgoing links answer (download links point at `releases/latest`).
