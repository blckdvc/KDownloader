### Install on Windows
Run `KDownloader_…_x64-setup.exe`.

### Install on Linux
Install the `.deb` (Debian, Ubuntu), or make the `.AppImage` executable and run it.
ZIP and 7z extraction needs `bsdtar` (package `libarchive-tools`), which the `.deb` installs.

### Install on macOS
1. Mount the `.dmg` and drag **KDownloader** to Applications.
2. Download `kdownloader-macos-sign.command` (attached below) and run it once:
   `./kdownloader-macos-sign.command`
   This locally signs the app so macOS lets it use the local network
   (needed for automatic UPnP port-forwarding). See the README for why.
3. Launch KDownloader, then allow it under
   System Settings → Privacy & Security → Local Network.
