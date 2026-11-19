# Calibration Client Linux

A pattern generator for Linux, controlled over the network by
[ColourSpace](https://www.lightillusion.com/colourspace.html). The program connects to
ColourSpace, receives the patches and shows them on the display being calibrated, in SDR
(8 or 10-bit), HDR10 (PQ) or HLG.

## Features

- **SDR, 8 or 10-bit:** the output follows the bit depth of the patches ColourSpace sends
  (a 10-bit surface through Vulkan when the system offers one, otherwise 8-bit).
- **HDR10 (BT.2020 PQ) and HLG:** 10-bit through Vulkan, with the patch levels sent unchanged.
- **Bypass KDE scaling:** on KDE Plasma, HDR patches reach the display without being scaled by
  the SDR brightness setting or tone-mapped by KWin (see below).
- **Connection window:** address, HDR on/off, signal and HDR metadata, with a check that HDR is
  really available (KDE: HDR switched on, an HDR-capable display).
- **Remembered settings:** the last address and HDR choices are offered again next time.

## Requirements

- Linux with SDL2.
- For 10-bit SDR and for HDR: a Vulkan driver (Mesa).
- For HDR: a Wayland session with HDR switched on for the display.
  - **KDE Plasma 6** (tested with KWin 6.8.3). `kscreen-doctor` is used to read KDE's HDR state
    and luminances.

## Building

```
git clone https://github.com/adolfotregosa/colourspace.git
cd colourspace
cargo build --release
```

The program is `target/release/calibrationclient`.

## Usage

```
./calibrationclient                     # opens the connection window
```

Without an address the **connection window** opens: enter ColourSpace's address, tick **HDR**
if wanted, choose the signal and press **Connect**. **Defaults** puts the HDR values (and Bypass KDE
scaling) back, **Cancel** quits.

With an address on the command line the window is skipped and the program does exactly what the
command line says (remembered settings are not used).

### In the patch window

- **Double-click:** fullscreen on/off (the mouse pointer is hidden in fullscreen).
- **Escape:** in fullscreen, back to a normal window; in a normal window, back to the connection
  window to change settings and reconnect.
- **Title bar:** shows the output in use (`HDR10`, `HLG`, `SDR 10-bit`, `SDR 8-bit`, ...) and
  whether ColourSpace is connected.

## HDR on KDE: Bypass KDE scaling

KWin scales HDR content by (its reference white) / (the reference white the application
declares). Its reference white is the **SDR brightness** setting, and Mesa always declares
203 cd/m² for a Vulkan HDR window. So PQ patches were only shown at their real level with SDR
brightness at exactly 203, which makes the desktop far too bright; at any other setting every
patch was off by SDR brightness / 203. KWin may also tone-map.

With **Bypass KDE scaling** (on by default, KDE only) the program describes the window's colours
itself, through the Wayland colour-management protocol (`wp_color_management_v1`):

- PQ or HLG, BT.2020, with **KWin's own reference white**, so the scale factor is exactly 1 and
  PQ reaches the display unchanged, whatever the SDR brightness;
- the **relative colorimetric** rendering intent, so KWin does not tone-map;
- KWin's own peak as the mastering peak, so KWin never clips below what the display can show.

A change of SDR brightness during a session is followed automatically.

With Bypass on, Primaries, the luminances, MaxCLL and MaxFALL are greyed out: KWin only uses
them to decide where to clip, so they could only lower the output. They are kept and used again
when Bypass is off, or on GNOME, where Mesa sends them on as before.

If the compositor lacks something this needs, nothing is changed and the program falls back to
Mesa's description; the reason is printed in the terminal. The terminal also prints what is
declared and what KWin reports (reference white, peak, black).

## Settings file

The last address and the HDR choices (checkbox, signal, primaries, the four luminance values and
Bypass KDE scaling) are kept in `calibrationclient.settings`, a plain text file in the folder
the program is started from, and nowhere else. Starting it from another folder gives that
folder its own settings. A missing or damaged file just means the built-in defaults are used.
