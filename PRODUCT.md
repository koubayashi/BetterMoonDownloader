# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

(Desktop app: Tauri v2 window rendered by WebView2 on Windows. The UI is web technology; the design language is desktop-native, not mobile.)

## Stack

Rust + Tauri v2. Rust owns extraction, downloading, and state; the UI is static HTML/CSS/JS with no bundler. Ships as a single Windows .exe. Chosen by the user over pure-native egui.

## Users

One person on their own Windows PC pulling large multi-part releases (game repacks split into dozens of `.rar` parts) from `fuckingfast.co` and `datanodes.to`. They paste or drop a `.txt` full of links and leave it running for minutes to hours, glancing back to check how far along it is.

## Product Purpose

BetterMoonDownloader is a Rust rewrite of moondownloader (LeyckerS/moondownloader, Python). It resolves share links to direct CDN links and downloads them in bulk. Success: a long batch finishes unattended, and at any glance the user knows what is done, what is left, how fast it is going, and when it will finish.

## Positioning

A dedicated download manager for these two hosts, which generic managers (IDM, FDM, JDownloader) can't resolve on their own. It should feel like those managers (queue, per-file progress, pause/resume, resume after restart, retries) while doing the host-specific extraction they can't.

## Operating Context

- Input: pasted links, or a `.txt` link file opened or dropped onto the window. FitGirl-style links carry the filename as a URL fragment.
- Both hosts sit behind Cloudflare Turnstile. The app drives a real Edge/Chrome window on a dedicated profile; the user may sometimes need to tick "Verify you are human" in it.
- A datanodes premium API key, when set, skips the browser for datanodes.
- Runs are long: minutes to hours, tens of GB.

## Capabilities and Constraints

- Per file: state, percentage, bytes done/total, live speed, ETA.
- Whole batch: files done/remaining, bytes done/total, aggregate speed, ETA based on current speed.
- Queue persists across restarts; `.tmp` partials resume with HTTP Range.
- Pause/resume, retry on failure, stall detection and re-extraction, stop the run when the disk is full.
- Modes: download, or links-only export.
- Settings: concurrency, retries, output folder, browser path, datanodes API key, captcha wait, proxies.

## Brand Commitments

- Name: **BetterMoonDownloader**. Lineage from Moon Downloader.
- Light and dark modes, following the OS theme by default.
- User-named references: Apple design, Impeccable, taste skill.

## Evidence on Hand

No real benchmarks for this rewrite yet. Don't make speed claims.

## Product Principles

1. Glanceable truth: progress, speed and ETA are always visible and honest.
2. Set and forget: a run survives restarts, flaky CDNs and captchas without babysitting.
3. Familiar download-manager behavior; invent only where these hosts demand it.
4. Quiet by default, loud only for things the user must act on (a captcha, a full disk).
