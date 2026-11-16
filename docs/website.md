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

## Screenshots to capture

The guide shows labelled placeholders (`.shot` figures) where real screenshots go. Capture them from the current release on real devices, hide channel keys and pairing codes, and replace each placeholder with an `<img>` with alt text.

| File | What it shows |
|------|---------------|
| `getting-started/a-radio-settings.png` | Meshtastic app: region, modem preset, channel name (keys hidden) |
| `getting-started/b-scan.png` | MeshGuard Bluetooth scan listing the T1000-E |
| `getting-started/b-connected.png` | Mesh Network screen with the radio connected |
| `getting-started/c-show-code.png` | Phone A showing the pairing QR code (code blurred) |
| `getting-started/c-scan.png` | Phone B scanning the code |
| `getting-started/c-paired.png` | Chats list with the new contact, Encrypted Chat |
| `getting-started/d-conversation.png` | A conversation with two messages and ✓✓ |
| `getting-started/e-checkin.png` | Check-in buttons, and the diagnostics sheet |

The home page has no app screenshot yet; it uses the radio-path illustration instead. Add a real one beside it once captured.

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
