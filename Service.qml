import QtQuick
import Quickshell
import Quickshell.Io

Item {
  id: root

  property var shell: null
  property var manifest: null
  property var pluginRegistry: null
  property bool initialized: false
  property bool binaryReady: false
  property bool syncPending: false
  property bool activationPending: false
  property bool ownershipClaimReady: false

  property string pluginVersion: ""
  property string target: ""
  property string asset: ""
  property string expectedChecksum: ""
  property string downloadPath: ""

  property string pluginDirectory: ""
  property string runtimeDirectory: ""
  property string binaryPath: ""
  property string temporaryPrefix: ""
  property string ownershipToken: ""

  readonly property string home: Quickshell.env("HOME")
  readonly property string themeNamePath: home + "/.local/state/omarchy/current/theme.name"
  readonly property string ownershipClaimPath: home
    + "/.local/state/omarchy/.zed-theme-owner"

  function fail(message) {
    console.warn("Omarchy Zed Theme: " + message)

    if (downloadPath && downloadPath.indexOf(temporaryPrefix) === 0) {
      cleanupProcess.command = ["rm", "-f", "--", downloadPath]
      cleanupProcess.running = true
      downloadPath = ""
    }

    notificationProcess.command = [
      "omarchy-notification-send",
      "Omarchy Zed Theme: " + message,
      "-t",
      "5000"
    ]
    notificationProcess.running = true
  }

  function initialize() {
    if (initialized || !manifest) return
    initialized = true
    pluginVersion = String(manifest.version || "")
    pluginDirectory = String(manifest.__sourceDir || "")

    if (!pluginVersion.match(/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/)) {
      fail("invalid plugin version")
      return
    }
    if (pluginDirectory.charAt(0) !== "/") {
      fail("cannot locate its plugin directory")
      return
    }

    runtimeDirectory = pluginDirectory + "/.runtime"
    binaryPath = runtimeDirectory + "/omarchy-zed-theme"
    temporaryPrefix = pluginDirectory.slice(0, pluginDirectory.lastIndexOf("/") + 1)
      + ".omarchy-zed-theme-download."
    ownershipToken = pluginVersion + "-" + Date.now().toString(36) + "-"
      + Math.random().toString(36).slice(2)
    ownershipClaimFile.setText(ownershipToken + "\n")
  }

  function continueInitialization() {
    if (ownershipClaimReady) return
    ownershipClaimReady = true
    architectureProcess.running = true
  }

  function requestSync() {
    syncPending = true
    if (binaryReady) syncTimer.restart()
  }

  function startSync() {
    if (!binaryReady || syncProcess.running) return
    syncPending = false
    syncProcess.command = [binaryPath]
    syncProcess.running = true
  }

  function beginActivation() {
    activationProcess.command = [binaryPath, "--activate", ownershipToken]
    activationProcess.running = true
  }

  function binaryVersionIsCurrent(exitCode, output) {
    return exitCode === 0
      && String(output || "").trim() === "omarchy-zed-theme " + pluginVersion
  }

  function acceptBinary() {
    binaryReady = true
    activationPending = true
    requestSync()
  }

  function expectedChecksumFrom(text) {
    var lines = String(text || "").split("\n")
    for (var i = 0; i < lines.length; i++) {
      var match = lines[i].match(/^([0-9A-Fa-f]{64})\s+\*?(.+)$/)
      if (match && match[2] === asset) return match[1].toLowerCase()
    }
    return ""
  }

  onManifestChanged: initialize()

  Component.onDestruction: {
    if (manifest && pluginRegistry && binaryPath
        && !pluginRegistry.isEnabled(String(manifest.id || ""))) {
      Quickshell.execDetached([binaryPath, "--restore", ownershipToken])
    }
  }

  FileView {
    id: ownershipClaimFile
    path: root.ownershipClaimPath
    blockWrites: true
    atomicWrites: true
    printErrors: false
    onSaved: root.continueInitialization()
    onSaveFailed: root.fail("cannot record service ownership")
  }

  FileView {
    path: root.themeNamePath
    watchChanges: true
    printErrors: false
    onFileChanged: {
      reload()
      root.requestSync()
    }
  }

  Timer {
    id: syncTimer
    interval: 10
    onTriggered: root.startSync()
  }

  Process {
    id: notificationProcess
  }

  Process {
    id: cleanupProcess
  }

  Process {
    id: architectureProcess
    command: ["uname", "-m"]
    stdout: StdioCollector {
      id: architectureOutput
      waitForEnd: true
    }
    onExited: function(exitCode) {
      if (exitCode !== 0) {
        root.fail("cannot detect CPU architecture")
        return
      }

      var architecture = String(architectureOutput.text || "").trim()
      if (architecture === "x86_64" || architecture === "amd64") {
        root.target = "x86_64-unknown-linux-musl"
      } else if (architecture === "aarch64" || architecture === "arm64") {
        root.target = "aarch64-unknown-linux-musl"
      } else {
        root.fail("unsupported CPU architecture: " + architecture)
        return
      }
      root.asset = "omarchy-zed-theme-" + root.target
      binaryExistsProcess.command = ["test", "-x", root.binaryPath]
      binaryExistsProcess.running = true
    }
  }

  Process {
    id: binaryExistsProcess
    onExited: function(exitCode) {
      if (exitCode === 0) {
        installedVersionProcess.command = [root.binaryPath, "--version"]
        installedVersionProcess.running = true
      } else {
        createRuntimeProcess.command = ["mkdir", "-p", root.runtimeDirectory]
        createRuntimeProcess.running = true
      }
    }
  }

  Process {
    id: installedVersionProcess
    stdout: StdioCollector {
      id: installedVersionOutput
      waitForEnd: true
    }
    onExited: function(exitCode) {
      if (root.binaryVersionIsCurrent(exitCode, installedVersionOutput.text)) {
        root.acceptBinary()
        return
      }

      createRuntimeProcess.command = ["mkdir", "-p", root.runtimeDirectory]
      createRuntimeProcess.running = true
    }
  }

  Process {
    id: activationProcess
    onExited: function(exitCode) {
      if (exitCode !== 0) {
        root.fail("cannot select the Omarchy theme in Zed")
      } else {
        root.activationPending = false
      }
    }
  }

  Process {
    id: createRuntimeProcess
    onExited: function(exitCode) {
      if (exitCode !== 0) {
        root.fail("cannot create its runtime directory")
        return
      }

      checksumDownloadProcess.command = [
        "curl", "--proto", "=https", "--tlsv1.2", "-fsSL", "--max-time", "60",
        "https://github.com/zharinov/omarchy-zed-theme/releases/download/v"
          + root.pluginVersion + "/SHA256SUMS"
      ]
      checksumDownloadProcess.running = true
    }
  }

  Process {
    id: checksumDownloadProcess
    stdout: StdioCollector {
      id: checksumDownloadOutput
      waitForEnd: true
    }
    onExited: function(exitCode) {
      if (exitCode !== 0) {
        root.fail("cannot download release checksums")
        return
      }

      root.expectedChecksum = root.expectedChecksumFrom(checksumDownloadOutput.text)
      if (!root.expectedChecksum) {
        root.fail("release checksum is missing")
        return
      }

      temporaryFileProcess.command = ["mktemp", root.temporaryPrefix + "XXXXXX"]
      temporaryFileProcess.running = true
    }
  }

  Process {
    id: temporaryFileProcess
    stdout: StdioCollector {
      id: temporaryFileOutput
      waitForEnd: true
    }
    onExited: function(exitCode) {
      root.downloadPath = String(temporaryFileOutput.text || "").trim()
      if (exitCode !== 0
          || root.downloadPath.indexOf(root.temporaryPrefix) !== 0) {
        root.downloadPath = ""
        root.fail("cannot create a temporary download")
        return
      }

      binaryDownloadProcess.command = [
        "curl", "--proto", "=https", "--tlsv1.2", "-fsSL", "--max-time", "120",
        "https://github.com/zharinov/omarchy-zed-theme/releases/download/v"
          + root.pluginVersion + "/" + root.asset,
        "-o", root.downloadPath
      ]
      binaryDownloadProcess.running = true
    }
  }

  Process {
    id: binaryDownloadProcess
    onExited: function(exitCode) {
      if (exitCode !== 0) {
        root.fail("cannot download its release binary")
        return
      }

      digestProcess.command = ["sha256sum", root.downloadPath]
      digestProcess.running = true
    }
  }

  Process {
    id: digestProcess
    stdout: StdioCollector {
      id: digestOutput
      waitForEnd: true
    }
    onExited: function(exitCode) {
      var actual = String(digestOutput.text || "").trim().split(/\s+/)[0].toLowerCase()
      if (exitCode !== 0 || actual !== root.expectedChecksum) {
        root.fail("release checksum verification failed")
        return
      }

      makeExecutableProcess.command = ["chmod", "755", root.downloadPath]
      makeExecutableProcess.running = true
    }
  }

  Process {
    id: makeExecutableProcess
    onExited: function(exitCode) {
      if (exitCode !== 0) {
        root.fail("cannot make its release binary executable")
        return
      }

      candidateVersionProcess.command = [root.downloadPath, "--version"]
      candidateVersionProcess.running = true
    }
  }

  Process {
    id: candidateVersionProcess
    stdout: StdioCollector {
      id: candidateVersionOutput
      waitForEnd: true
    }
    onExited: function(exitCode) {
      if (!root.binaryVersionIsCurrent(exitCode, candidateVersionOutput.text)) {
        root.fail("release binary has an unexpected version")
        return
      }

      publishBinaryProcess.command = ["mv", "-f", root.downloadPath, root.binaryPath]
      publishBinaryProcess.running = true
    }
  }

  Process {
    id: publishBinaryProcess
    onExited: function(exitCode) {
      if (exitCode !== 0) {
        root.fail("cannot publish its release binary")
        return
      }

      root.downloadPath = ""
      root.acceptBinary()
    }
  }

  Process {
    id: syncProcess
    stderr: StdioCollector {
      id: syncErrorOutput
      waitForEnd: true
    }
    onExited: function(exitCode) {
      if (exitCode !== 0) {
        console.warn("Omarchy Zed Theme sync error: "
          + String(syncErrorOutput.text || "").trim())
        root.fail("cannot update Zed")
      } else if (root.activationPending && !root.syncPending
          && !activationProcess.running) {
        root.beginActivation()
      }

      if (root.syncPending) syncTimer.restart()
    }
  }
}
