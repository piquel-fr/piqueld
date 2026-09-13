{
  pkgs,
  module,
  daemon,
  cli,
}:
pkgs.testers.runNixOSTest {
  name = "piqueld-service";
  nodes.machine = { ... }: {
    imports = [ module ];
    services.piqueld = {
      enable = true;
      package = daemon;
      cliPackage = cli;
      dataDir = "/var/lib/piqueld-test";
    };
    environment.systemPackages = [ pkgs.curl ];
    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 4096;
  };
  testScript = ''
    start_all()
    machine.wait_for_unit("piqueld.service")
    machine.wait_until_succeeds("curl --fail --unix-socket /var/lib/piqueld-test/piqueld.sock http://localhost/api/v1/system/status")
    machine.succeed("test $(stat -c %a /var/lib/piqueld-test) = 700")
    machine.succeed("test $(stat -c %a /var/lib/piqueld-test/piqueld.sock) = 600")
    machine.fail("su nobody -s /bin/sh -c 'cat /var/lib/piqueld-test/piqueld.db'")
    machine.succeed("piquelctl --socket /var/lib/piqueld-test/piqueld.sock status")
    machine.succeed("docker info --format '{{.Swarm.ControlAvailable}}' | grep true")
    machine.succeed("systemctl restart piqueld.service")
    machine.wait_for_unit("piqueld.service")
    machine.wait_until_succeeds("piquelctl --socket /var/lib/piqueld-test/piqueld.sock status")
  '';
}
