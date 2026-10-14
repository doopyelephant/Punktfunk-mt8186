# Boots the NixOS module with the real packages. The host, console and runner units come up from
# the packages' own files plus the module's drop-ins, and the runner keeps the sandbox its shipped
# unit declares. It builds the host, so only the `ci:nix-rust` lane of nix.yml builds this.
{
  testers,
  module,
}:
testers.runNixOSTest {
  name = "punktfunk-module";

  nodes.machine = {
    imports = [ module ];
    services.punktfunk.host = {
      enable = true;
      users = [ "alice" ];
      autoStart = true;
      # A gamescope build of its own, and nothing here needs it.
      gamescopeHdr = false;
    };
    # Lingering starts alice's manager at boot, as on an appliance.
    users.users.alice = {
      isNormalUser = true;
      linger = true;
    };
    virtualisation.memorySize = 2048;
  };

  testScript = ''
    def show(unit, *props):
        args = " ".join(f"-p {p}" for p in props)
        return machine.succeed(f"systemctl --user -M alice@ show {unit} {args}")

    machine.wait_for_unit("multi-user.target")
    for unit in ("punktfunk-host", "punktfunk-web", "punktfunk-scripting"):
        machine.wait_for_unit(f"{unit}.service", "alice")
    # The console serves only once the host has written its token and certificate.
    machine.wait_for_open_port(47990)
    machine.wait_for_open_port(47992)

    # KWin identifies the host by its store path; a wrapper there breaks desktop streaming.
    exec_start = show("punktfunk-host", "ExecStart")
    assert "-punktfunk-host-" in exec_start and "/run/wrappers" not in exec_start, exec_start

    sandbox = show(
        "punktfunk-scripting",
        "NoNewPrivileges",
        "ProtectSystem",
        "ProtectHome",
        "PrivateTmp",
        "ProtectControlGroups",
        "CapabilityBoundingSet",
        "RestartPreventExitStatus",
        "InaccessiblePaths",
    )
    for want in (
        "NoNewPrivileges=yes",
        "ProtectSystem=strict",
        "ProtectHome=tmpfs",
        "PrivateTmp=no",
        "ProtectControlGroups=yes",
        "CapabilityBoundingSet=\n",
        "RestartPreventExitStatus=78",
        "/home/alice/.config/punktfunk/mgmt-token",
    ):
        assert want in sandbox, f"{want!r} missing from:\n{sandbox}"
  '';
}
