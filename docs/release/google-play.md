# Publishing MeshGuard on Google Play

This guide covers the one-time setup (signing key, Play Console) and the
per-release flow. Policy numbers below (tester count, target API) change over
time: confirm them in Play Console when you get there.

## 1. Create the permanent signing key (once)

Android only installs an update when it is signed with the same key as the
installed app. The release workflow therefore refuses to build without a
stable keystore.

```bash
keytool -genkeypair -v \
  -keystore meshguard-release.jks \
  -alias meshguard \
  -keyalg RSA -keysize 4096 -validity 10000 \
  -dname "CN=MeshGuard"
```

- Store `meshguard-release.jks` and both passwords in a password manager and
  keep an offline backup. **If the key is lost, existing installs can never be
  updated again.** Keep it outside the checkout; `.gitignore` also excludes
  `*.jks` and `*.keystore` as a safety net.
- Print the certificate fingerprint, needed for IzzyOnDroid and Play:

  ```bash
  keytool -list -v -keystore meshguard-release.jks -alias meshguard | grep SHA256
  ```

### GitHub secrets

Repository → Settings → Secrets and variables → Actions → New repository secret:

| Secret | Value |
|---|---|
| `ANDROID_KEYSTORE_BASE64` | `base64 -w0 meshguard-release.jks` |
| `ANDROID_KEYSTORE_PASSWORD` | keystore password |
| `ANDROID_KEY_ALIAS` | `meshguard` |
| `ANDROID_KEY_PASSWORD` | key password |

With these set, every tag release signs the APK (GitHub, IzzyOnDroid) and the
AAB (Google Play) with this key.

### Existing installs and IzzyOnDroid

Releases up to `v2026.11.14` were signed with a throwaway key generated in CI.
Users of those builds have to uninstall once before installing a release signed
with the permanent key. IzzyOnDroid verifies the signing certificate, so open
an issue on the IzzyOnDroid repository with the new SHA-256 fingerprint and ask
them to accept the key change for `com.meshguard.app`.

## 2. Google Play developer account (once)

1. Register at <https://play.google.com/console> (one-time fee, identity
   verification). Choose a **personal** or **organization** account; the name is
   shown publicly as the developer.
2. **New personal accounts** must run a *closed test* with at least 12 testers
   who stay opted in for 14 consecutive days before production access is
   granted. Organization accounts are exempt. Start recruiting testers early;
   they need a Google account and an Android phone.

## 3. Create the app in Play Console

1. *Create app* → name **MeshGuard**, default language English, App, Free.
2. **App signing.** Upload the first AAB (see §4). Play App Signing is
   mandatory; accept the Google-generated app signing key. The key from §1
   becomes the **upload key**.
   - Consequence: the Play build and the GitHub/IzzyOnDroid APK have different
     signatures, so a user cannot switch between them without reinstalling.
     This is the normal setup for apps distributed in several stores.
3. **App content** (Policy → App content):

   | Form | Answer |
   |---|---|
   | Privacy policy | `https://www.meshguard.net/privacy` |
   | Ads | No ads |
   | App access | All functionality available without special access. Note for reviewers: a Meshtastic radio is needed to send messages; the app opens and scans without one |
   | Content rating | Questionnaire: communication app, users can exchange messages; no violence, gambling, etc. |
   | Target audience | 18+ (avoids the Families policy requirements) |
   | Data safety | No data collected, no data shared. Data is encrypted in transit: yes. Users can request deletion: not applicable (nothing is collected; uninstall removes local data) |
   | Government apps / Financial features / Health | No |

4. **Store listing:** short and full description from
   `fastlane/metadata/android/en-US/`, the 512×512 icon, a 1024×500 feature
   graphic and at least two phone screenshots.
5. **Encryption / export.** Play does not ask for an export filing. MeshGuard's
   source is public, which under US EAR §742.15(b) normally needs only a
   one-time email notification to BIS and the NSA. Check this if you publish as
   an organization.

## 4. Per-release flow

1. Tag a release as usual (`git tag vYYYY.MM.DD && git push origin vYYYY.MM.DD`).
2. The *Android APK* job builds:
   - `meshguard-YYYYMMDD-android.apk` → attached to the GitHub release;
   - `meshguard-YYYYMMDD.aab` → workflow artifact **android-aab**.

   It also fails the build if any native library is not 16 KB page-aligned
   (required for apps targeting Android 15+).
3. Download the `android-aab` artifact from the workflow run and upload it in
   Play Console → Testing → Closed testing (or Internal testing) → Create release.
4. After the closed test requirement is met, apply for production access and
   promote the release.

`versionCode` is derived from the tag (`2026.11.14` → `2026011014`), so it
always increases as long as tags are date-based.

### Optional: automatic upload

Once the app exists in Play Console and one AAB has been uploaded manually:

1. Google Cloud Console → create a service account → JSON key.
2. Play Console → Users and permissions → invite the service account email
   with *Release to testing tracks* permission for MeshGuard.
3. Add the JSON as the GitHub secret `PLAY_SERVICE_ACCOUNT_JSON`.

Each tag release then uploads the AAB to the **internal** track as a **draft**.
Nothing reaches users until you roll it out in Play Console.

## 5. After the app is live

Add the Google Play badge to `README.md` (Downloads → Android) and to the
Android card on `website/index.html`, linking to
`https://play.google.com/store/apps/details?id=com.meshguard.app`. Use the
official badge from <https://play.google.com/intl/en_us/badges/> and follow
Google's badge guidelines.
