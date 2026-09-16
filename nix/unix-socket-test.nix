{
  pkgs,
  daemon,
  cli,
}:
pkgs.testers.runNixOSTest {
  name = "piqueld-unix-socket";

  nodes.machine = { ... }: {
    imports = [ ./module.nix ];
    services.piqueld = {
      enable = true;
      package = daemon;
    };
    users.users = {
      operator = {
        isNormalUser = true;
        extraGroups = [ "piqueld" ];
      };
      outsider.isNormalUser = true;
    };
    environment.systemPackages = [ cli ];
    virtualisation.memorySize = 2048;
  };

  testScript = ''
    start_all()
    machine.wait_for_unit("piqueld.service")
    machine.wait_until_succeeds("runuser -u operator -- piquelctl status")

    with subtest("group access without private-state or directory write access"):
        machine.succeed("test $(stat -c %a /run/piqueld) = 750")
        machine.succeed("test $(stat -c %a /run/piqueld/piqueld.sock) = 660")
        machine.succeed("test $(stat -c %U:%G /run/piqueld/piqueld.sock) = piqueld:piqueld")
        machine.succeed("test $(stat -c %a /var/lib/piqueld) = 700")
        machine.fail("runuser -u outsider -- piquelctl status")
        machine.fail("runuser -u operator -- cat /var/lib/piqueld/piqueld.db")
        machine.fail("runuser -u operator -- touch /run/piqueld/unwanted")
        machine.fail("runuser -u operator -- rm /run/piqueld/piqueld.sock")

    with subtest("one piqueld service and no socket unit"):
        units = machine.succeed("systemctl list-unit-files 'piqueld*' --no-legend --no-pager").splitlines()
        assert [line.split()[0] for line in units if line.strip()] == ["piqueld.service"], units
        machine.succeed("test $(systemctl show piqueld.service -p UMask --value) = 0077")

    with subtest("a daemon with different state cannot replace the live socket"):
        machine.succeed("install -d -o piqueld -g piqueld -m 0700 /var/lib/piqueld-second")
        machine.succeed("printf '[server]\\ndata_dir = \"/var/lib/piqueld-second\"\\n' > /tmp/second.toml")
        machine.fail("runuser -u piqueld -- ${daemon}/bin/piqueld --config /tmp/second.toml > /tmp/second.log 2>&1")
        machine.succeed("grep 'failed to lock runtime directory' /tmp/second.log")
        machine.succeed("test ! -e /var/lib/piqueld-second/piqueld.db")
        machine.succeed("runuser -u operator -- piquelctl status")

    with subtest("runtime directory ownership and group validation"):
        machine.succeed("install -d -o root -g piqueld -m 0750 /run/piqueld-unsafe")
        machine.succeed("printf '[server]\\ndata_dir = \"/var/lib/piqueld-second\"\\nruntime_dir = \"/run/piqueld-unsafe\"\\n' > /tmp/unsafe.toml")
        machine.fail("runuser -u piqueld -- ${daemon}/bin/piqueld --config /tmp/unsafe.toml > /tmp/unsafe.log 2>&1")
        machine.succeed("grep 'must be owned by uid' /tmp/unsafe.log")
        machine.succeed("chown piqueld:users /run/piqueld-unsafe")
        machine.fail("runuser -u piqueld -- ${daemon}/bin/piqueld --config /tmp/unsafe.toml > /tmp/unsafe.log 2>&1")
        machine.succeed("grep 'must be owned by gid' /tmp/unsafe.log")

    with subtest("stale socket recovery"):
        machine.succeed("systemctl stop piqueld.service")
        machine.succeed("install -d -o piqueld -g piqueld -m 0750 /run/piqueld")
        machine.succeed("runuser -u piqueld -- ${pkgs.python3}/bin/python -c 'import socket; s = socket.socket(socket.AF_UNIX); s.bind(\"/run/piqueld/piqueld.sock\")'")
        machine.succeed("systemctl start piqueld.service")
        machine.wait_until_succeeds("runuser -u operator -- piquelctl status")
  '';
}
