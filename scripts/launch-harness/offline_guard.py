"""Guardia «zero richieste» del lancio offline (onda 8a, D138/D139; onda 8b, D137).

Lancia il binario vero del launcher, in modalità automazione, con l'account
attivo che gioca offline e con `HTTP(S)_PROXY`/`ALL_PROXY` puntati su un proxy
locale che **logga e rifiuta** ogni richiesta. `reqwest` legge quelle variabili
per tutte le forme di client usate nel repo (trappola 17 della wiki): il log del
proxy è la lista delle richieste che il processo ha tentato.

Un log vuoto è anche quello di un lancio che non è mai partito, quindi lo
scenario verde asserisce **due cose insieme**: il log è vuoto *e* il finto
`java` ha scritto i suoi argomenti — con la classe principale di Minecraft — in
un file. Il file esiste solo se la pipeline è arrivata a lanciare il gioco. Il
finto `java` è anche la rete di sicurezza: risponde a `-version` per superare
l'ispezione, e per ogni altra chiamata scrive gli argomenti ed esce con 1, quindi
Minecraft non parte mai.

Scenari (account offline, cache reale in hard link):

- `vanilla`            — cache completa; atteso: 0 richieste, `java` raggiunto,
                         nessun file nella cache dei metadati del loader.
- `fabric-jar-missing` — un jar registrato tolto dalla cache del workspace.
- `fabric-complete`    — cache completa.

I due scenari Fabric hanno due fasi sullo **stesso** workspace (D137):

- `priming` — il proxy inoltra alla rete vera **solo** `meta.fabricmc.net` e
              rifiuta il resto; atteso: 1 sola richiesta, `meta.fabricmc.net`,
              `java` raggiunto, e la voce di cache scritta dal lancio in
              `cache/loader-metadata/<mc>/fabric.json`. La voce la scrive il
              codice di produzione con la risposta della rete vera.
- `measure` — il proxy rifiuta tutto; atteso: 0 richieste, `java` raggiunto.

Controlli negativi, eseguiti a ogni giro perché senza di loro il conteggio non
prova niente:

- `control-proxy-sees`  — vanilla senza `version.json` nella cache del
                          workspace: il lancio **deve** chiedere il manifest, e il
                          log deve contenerlo. Se è vuoto, il proxy non è nel giro.
- `control-closed-port` — come sopra, ma il launcher punta a una porta chiusa
                          mentre il proxy ascolta sulla sua: il log deve restare
                          **vuoto** e il lancio fallire. Le righe arrivano solo dal
                          traffico che passa dal proxy.
- `control-no-cache-entry` — Fabric, `priming`, poi la sola voce di cache tolta
                          dal workspace e la misura: 0 richieste, `java` non
                          raggiunto, e il motivo dice che la combinazione non è mai
                          stata lanciata online. Se resta verde, il verde non
                          veniva dalla cache.
- `control-online-still-fetches` — Fabric, `priming` due volte: anche con la voce
                          presente la seconda chiede ancora `meta.fabricmc.net`
                          (la cache non vince mai sulla rete, D137 vincolo 2).

Esce con 0 se ogni scenario e ogni controllo combacia con l'atteso, 1 altrimenti.

Dati veri: il proxy rifiuta tutto tranne il meta di Fabric nella fase di
innesco, quindi nessun download riesce e nessun file in hard link viene
riscritto da un download (trappola 10). La cache dei metadati del loader non è
mai collegata ai dati veri: nasce nel workspace, e prima e dopo ogni fase
`assert_private` la verifica (nlink 1). Si copiano il DB (con
le righe account cancellate), i JSON della mod list, i `custom_configs`, le
librerie dell'istanza e i file piccoli accanto al `client.jar` (`options-keys.json`
viene riscritto in place da `options_keys::load_or_derive`). Si hard-linkano
`cache/minecraft`, `cache/mods`, `cache/content-packs`, `cache/configs` e i
`local-jars` della mod list. Prima e dopo un giro su dati veri, il manifesto
sha1 dell'albero reale che gli hard link toccano:

    cd ~/.local/share/com.cubic.launcher
    find cache "mod-lists/Drehmal APOTHEOSIS/local-jars" launcher_data.db -type f -print0 \
        | xargs -0 sha1sum > ~/.cache/cubic-offline-guard/real-sha1-before.txt
    sha1sum -c --quiet ~/.cache/cubic-offline-guard/real-sha1-before.txt

Display: `broadwayd` (GTK3) se c'è, altrimenti `xvfb-run`. Il frontend non
viene servito (binario debug, `devUrl` su :1420 irraggiungibile): la guardia
misura il lancio, non le richieste dell'app all'avvio (onda 8c).

Locale e advisory: in CI vorrebbe una cache di Minecraft come fixture.

stdlib-only.
"""

from __future__ import annotations

import argparse
import datetime
import json
import os
import select
import shutil
import socket
import socketserver
import subprocess
import sys
import threading
import time
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path

from run_combo import ComboSpec, run_combo
from workspace import HarnessWorkspace, vanilla_modlist

DEFAULT_REAL_DATA = Path.home() / ".local/share/com.cubic.launcher"
DEFAULT_WORK_DIR = Path.home() / ".cache/cubic-offline-guard"
DEFAULT_MODLIST = "Drehmal APOTHEOSIS"
REPO_ROOT = Path(__file__).resolve().parents[2]
# Il binario debug, esplicito: `run_combo` sceglierebbe un release se c'è.
DEFAULT_BINARY = REPO_ROOT / "src-tauri/target/debug/cubic_launcher"
# Oltre a `src-tauri/src/`: quello che, cambiato, cambia il binario.
SOURCE_FILES = ("src-tauri/Cargo.toml", "src-tauri/Cargo.lock", "src-tauri/tauri.conf.json")
DEFAULT_MC = "1.20.1"
DEFAULT_MISSING_JAR = "fabric/unerR5MN/ferritecore-6.0.1-fabric.jar"
VANILLA_MODLIST = "offline-guard-vanilla"
# Il `devUrl` del binario debug (`tauri.conf.json`): se qui risponde qualcuno,
# la webview carica il frontend e le sue richieste finiscono nel log.
DEV_SERVER_PORT = 1420
# I JSON della mod list che il lancio può riscrivere: si copiano (trappola 9).
MODLIST_JSON_FILES = (
    "rules.json",
    "resourcepacks.json",
    "datapacks.json",
    "shaders.json",
    "modlist-editor-groups.json",
)
FAKE_JAVA_ARGS = "fake-java-args.txt"
# La cache dei metadati del loader (D137, onda 8b): una voce per combinazione,
# `<LOADER_CACHE_DIR>/<mc>/<loader>.json`. Il harness non la scrive mai: la
# cerca, la toglie (controllo) e la verifica privata.
LOADER_CACHE_DIR = "cache/loader-metadata"
FABRIC_META_HOST = "meta.fabricmc.net"
# Il pezzo fisso del messaggio di D137, vincolo 3: offline, combinazione mai lanciata online.
NEVER_LAUNCHED_ONLINE = "has never been launched online"


# ---- proxy che logga, e rifiuta o inoltra -------------------------------------


def request_host(line: str) -> str | None:
    """The host of a logged request line: `CONNECT h:443 ...` or `GET http://h/...`."""
    request = line.split()
    if len(request) < 2:
        return None
    target = request[1]
    if "://" in target:
        target = target.split("://", 1)[1].split("/", 1)[0]
    return target.rsplit(":", 1)[0] if ":" in target else target


def _tunnel(client: socket.socket, upstream: socket.socket) -> None:
    """Copy bytes both ways until one side closes or both stay idle for 60 s."""
    sockets = [client, upstream]
    while True:
        readable, _, _ = select.select(sockets, [], [], 60)
        if not readable:
            return
        for source in readable:
            data = source.recv(65536)
            if not data:
                return
            (upstream if source is client else client).sendall(data)


class _LoggingHandler(socketserver.StreamRequestHandler):
    def handle(self) -> None:
        line = self.rfile.readline(65536).decode("latin-1").strip()
        stamp = datetime.datetime.now().isoformat(timespec="milliseconds")
        with self.server.log_lock, open(self.server.log_path, "a", encoding="utf-8") as log:
            log.write(f"{stamp}\t{line}\n")
        request = line.split()
        host = request_host(line)
        if request and request[0] == "CONNECT" and host in self.server.forward_hosts:
            # Il client non manda niente prima del 200: dopo le intestazioni il
            # buffer di `rfile` è vuoto, e il resto passa dal socket nudo.
            while self.rfile.readline(65536) not in (b"\r\n", b"\n", b""):
                pass
            port = int(request[1].rsplit(":", 1)[1])
            with socket.create_connection((host, port), timeout=30) as upstream:
                self.wfile.write(b"HTTP/1.1 200 Connection established\r\n\r\n")
                self.wfile.flush()
                _tunnel(self.connection, upstream)
            return
        self.wfile.write(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")


class LoggingProxy(socketserver.ThreadingTCPServer):
    """Logs the first line of every request (CONNECT host:443 / GET http://...).

    Answers 403 to everything, except a `CONNECT` to one of `forward_hosts`,
    which is tunnelled to the real network. Con `forward_hosts` vuoto non apre
    mai una connessione verso l'esterno.
    """

    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, log_path: Path, forward_hosts: tuple[str, ...] = ()):
        super().__init__(("127.0.0.1", 0), _LoggingHandler)
        self.log_path = log_path
        self.forward_hosts = frozenset(forward_hosts)
        self.log_lock = threading.Lock()
        log_path.write_text("")
        self._thread = threading.Thread(target=self.serve_forever, daemon=True)

    @property
    def port(self) -> int:
        return self.server_address[1]

    def __enter__(self) -> "LoggingProxy":
        self._thread.start()
        return self

    def __exit__(self, *_exc) -> None:
        self.shutdown()
        self.server_close()


def requested_hosts(log_path: Path) -> Counter:
    """Count logged requests per host."""
    hosts: Counter = Counter()
    for line in log_path.read_text(encoding="utf-8").splitlines():
        hosts[request_host(line.split("\t", 1)[-1]) or "<malformed>"] += 1
    return hosts


def closed_port() -> int:
    """A local port nobody listens on: bound, then released."""
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def port_is_open(port: int) -> bool:
    with socket.socket() as probe:
        probe.settimeout(0.5)
        return probe.connect_ex(("127.0.0.1", port)) == 0


# ---- workspace ------------------------------------------------------------------


def write_fake_java(ws: HarnessWorkspace) -> Path:
    """`-version` passes the inspection as Java 17; anything else is recorded and exits 1."""
    script = ws.launcher_root / "fake-java"
    args_file = ws.launcher_root / FAKE_JAVA_ARGS
    script.write_text(
        "#!/bin/sh\n"
        "# Finto java della guardia offline: non avvia niente.\n"
        'if [ "$1" = "-version" ]; then\n'
        "  echo 'openjdk version \"17.0.99\" 2026-01-01' >&2\n"
        "  echo 'OpenJDK 64-Bit Server VM (build 17.0.99, mixed mode)' >&2\n"
        "  exit 0\n"
        "fi\n"
        f"printf '%s\\n' \"$@\" > '{args_file}'\n"
        "exit 1\n"
    )
    script.chmod(0o755)
    return script


def build_workspace(args: argparse.Namespace, scenario: "Scenario") -> HarnessWorkspace:
    real = args.real_data
    ws = HarnessWorkspace(keep=args.keep, base_dir=args.work_dir)
    ws.adopt_database(real / "launcher_data.db")
    ws.seed_offline_account(
        username="OfflineGuard", microsoft_id="offline-guard", plays_offline=True
    )
    ws.set_global_setting("java_path_override", str(write_fake_java(ws)))

    for sub in ("minecraft", "mods", "content-packs", "configs"):
        if (real / "cache" / sub).is_dir():
            ws.link_tree(real / "cache" / sub, f"cache/{sub}")
    version_dir = f"cache/minecraft/{args.mc}"
    if not (ws.launcher_root / version_dir / "client.jar").is_file():
        raise RuntimeError(f"no cached client.jar for Minecraft {args.mc} under {real}")
    # Accanto al jar: file piccoli che il lancio può riscrivere in place.
    for path in (ws.launcher_root / version_dir).iterdir():
        if path.is_file() and path.suffix != ".jar":
            ws.privatize(f"{version_dir}/{path.name}")
            ws.assert_private(f"{version_dir}/{path.name}")

    if scenario.loader == "vanilla":
        ws.create_modlist(vanilla_modlist(VANILLA_MODLIST))
    else:
        source = real / "mod-lists" / args.modlist
        target = f"mod-lists/{args.modlist}"
        for name in (*MODLIST_JSON_FILES, "options.txt"):
            if (source / name).is_file():
                ws.copy_from(source / name, f"{target}/{name}")
        if (source / "custom_configs").is_dir():
            ws.copy_from(source / "custom_configs", f"{target}/custom_configs")
        if (source / "local-jars").is_dir():
            ws.link_tree(source / "local-jars", f"{target}/local-jars")
        libraries = source / "instances" / f"{args.mc}-{scenario.loader}" / "libraries"
        if libraries.is_dir():
            ws.copy_from(libraries, f"{target}/instances/{args.mc}-{scenario.loader}/libraries")
        ws.assert_private(f"{target}/instances")
        for name in (*MODLIST_JSON_FILES, "options.txt", "custom_configs"):
            if (ws.launcher_root / target / name).exists():
                ws.assert_private(f"{target}/{name}")
    ws.assert_private("launcher_data.db")

    if scenario.remove_jar:
        jar = ws.launcher_root / "cache/mods" / args.missing_jar
        if not jar.is_file():
            raise RuntimeError(f"the jar to remove is not in the cache: {jar}")
        jar.unlink()  # solo il nome nel workspace: l'inode reale resta
    if scenario.remove_version_json:
        (ws.launcher_root / version_dir / "version.json").unlink()
    # La cache dei metadati del loader non viene mai dai dati veri: la fase di
    # innesco ci SCRIVE, e un hard link porterebbe la scrittura nei dati veri.
    # Nasce vuota qui, la crea il lancio, e `run_phase` la verifica privata.
    if (ws.launcher_root / LOADER_CACHE_DIR).exists():
        raise RuntimeError(f"REFUSING: {LOADER_CACHE_DIR} exists before the first launch")
    return ws


# ---- scenari --------------------------------------------------------------------


@dataclass
class Phase:
    """One launch on the scenario's workspace. Le fasi di uno scenario condividono il workspace."""

    label: str
    # Richieste attese per host. `None` = almeno una richiesta a `must_see`.
    expected_hosts: dict[str, int] | None
    expect_java: bool
    must_see: str | None = None
    # Host che il proxy inoltra alla rete vera; tutto il resto è rifiutato.
    forward_hosts: tuple[str, ...] = ()
    proxy_on_closed_port: bool = False
    # Toglie dal workspace solo la voce di cache della combinazione, prima del lancio.
    drop_loader_cache_entry: bool = False
    # True: la voce deve esserci (prima / dopo il lancio). False: nessun file
    # sotto `LOADER_CACHE_DIR`. None: non si guarda.
    loader_cache_before: bool | None = None
    loader_cache_after: bool | None = None
    # Pezzi che il motivo del fallimento deve contenere; `{mc}` è la versione.
    failure_must_contain: tuple[str, ...] = ()


@dataclass
class Scenario:
    name: str
    loader: str
    phases: list[Phase]
    remove_jar: bool = False
    remove_version_json: bool = False


def priming(label: str = "priming", loader_cache_before: bool | None = None) -> Phase:
    """Fase 1: account offline, il proxy inoltra solo il meta di Fabric.

    La voce di cache che la fase 2 rilegge la scrive il codice di produzione,
    con la risposta della rete vera: nessuna fixture, nessun formato del harness.
    """
    return Phase(
        label,
        expected_hosts={FABRIC_META_HOST: 1},
        expect_java=True,
        forward_hosts=(FABRIC_META_HOST,),
        loader_cache_before=loader_cache_before,
        loader_cache_after=True,
    )


MEASURE = Phase("measure", expected_hosts={}, expect_java=True)

SCENARIOS = [
    Scenario(
        "vanilla",
        "vanilla",
        [Phase("offline", expected_hosts={}, expect_java=True, loader_cache_after=False)],
    ),
    Scenario("fabric-jar-missing", "fabric", [priming(), MEASURE], remove_jar=True),
    Scenario("fabric-complete", "fabric", [priming(), MEASURE]),
]
CONTROLS = [
    Scenario(
        "control-proxy-sees",
        "vanilla",
        [
            Phase(
                "offline",
                expected_hosts=None,
                must_see="piston-meta.mojang.com",
                expect_java=False,
            )
        ],
        remove_version_json=True,
    ),
    Scenario(
        "control-closed-port",
        "vanilla",
        [Phase("offline", expected_hosts={}, expect_java=False, proxy_on_closed_port=True)],
        remove_version_json=True,
    ),
    # La misura senza la voce di cache: se resta verde, il verde non veniva dalla cache.
    Scenario(
        "control-no-cache-entry",
        "fabric",
        [
            priming(),
            Phase(
                "measure-without-entry",
                expected_hosts={},
                expect_java=False,
                drop_loader_cache_entry=True,
                failure_must_contain=(NEVER_LAUNCHED_ONLINE, "Fabric", "{mc}"),
            ),
        ],
    ),
    # D137, vincolo 2: con la voce presente, il percorso che chiede alla rete chiede ancora.
    Scenario(
        "control-online-still-fetches",
        "fabric",
        [priming(), priming("priming-again", loader_cache_before=True)],
    ),
]


@dataclass
class Outcome:
    name: str
    passed: bool
    hosts: Counter
    java_reached: bool
    state: str
    failure: str | None
    reasons: list[str] = field(default_factory=list)
    log_lines: list[str] = field(default_factory=list)
    launch_log_dir: str | None = None


def launcher_env(ws: HarnessWorkspace, proxy_port: int, display_env: dict[str, str]) -> dict[str, str]:
    proxy = f"http://127.0.0.1:{proxy_port}"
    env = {
        "HOME": os.environ["HOME"],
        "PATH": "/usr/bin:/bin",
        "LANG": "C.UTF-8",
        "XDG_DATA_HOME": str(ws.xdg_data_home),
        "WEBKIT_DISABLE_DMABUF_RENDERER": "1",
        # Un bus inesistente: il portachiavi vero non viene nemmeno contattato.
        "DBUS_SESSION_BUS_ADDRESS": "unix:path=/nonexistent-offline-guard",
        # La webview carica `devUrl` da localhost: va diretta, e trova chiuso.
        "NO_PROXY": "localhost,127.0.0.1,::1",
        "no_proxy": "localhost,127.0.0.1,::1",
    }
    for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
        env[name] = proxy
        env[name.lower()] = proxy
    if "XDG_RUNTIME_DIR" in os.environ:
        env["XDG_RUNTIME_DIR"] = os.environ["XDG_RUNTIME_DIR"]
    env.update(display_env)
    return env


def loader_cache_entry(args: argparse.Namespace, loader: str) -> str:
    return f"{LOADER_CACHE_DIR}/{args.mc}/{loader}.json"


def loader_cache_reasons(
    ws: HarnessWorkspace, entry: str, expected: bool | None, when: str
) -> list[str]:
    """Check the loader metadata cache, and that it is private (nlink 1) whenever it exists."""
    cache_dir = ws.launcher_root / LOADER_CACHE_DIR
    if cache_dir.exists():
        ws.assert_private(LOADER_CACHE_DIR)
    if expected is True and not (ws.launcher_root / entry).is_file():
        return [f"no loader metadata cache entry {entry} {when}"]
    if expected is False and cache_dir.exists() and any(p.is_file() for p in cache_dir.rglob("*")):
        return [f"files under {LOADER_CACHE_DIR} {when}, none expected"]
    return []


def run_phase(
    args: argparse.Namespace,
    ws: HarnessWorkspace,
    scenario: Scenario,
    phase: Phase,
    display: str,
    display_env: dict,
) -> Outcome:
    name = scenario.name if len(scenario.phases) == 1 else f"{scenario.name}/{phase.label}"
    entry = loader_cache_entry(args, scenario.loader)
    reasons: list[str] = []
    if phase.drop_loader_cache_entry:
        if (ws.launcher_root / entry).is_file():
            (ws.launcher_root / entry).unlink()  # solo questo file
        else:
            reasons.append(f"no loader metadata cache entry {entry} to remove")
    reasons += loader_cache_reasons(ws, entry, phase.loader_cache_before, "before the launch")

    args_file = ws.launcher_root / FAKE_JAVA_ARGS
    args_file.unlink(missing_ok=True)  # quello di una fase precedente non conta
    log_path = ws.xdg_data_home / f"proxy-{name.replace('/', '-')}.log"
    with LoggingProxy(log_path, phase.forward_hosts) as proxy:
        port = closed_port() if phase.proxy_on_closed_port else proxy.port
        modlist = VANILLA_MODLIST if scenario.loader == "vanilla" else args.modlist
        spec = ComboSpec(
            loader=scenario.loader,
            mc_version=args.mc,
            modlist_name=modlist,
            timeout_seconds=args.timeout,
            success_after_seconds=5,
        )
        result = run_combo(
            spec,
            ws,
            binary=args.binary,
            display=display,
            base_env=launcher_env(ws, port, display_env),
        )
        time.sleep(0.5)  # le ultime righe del proxy
    hosts = requested_hosts(log_path)
    main_class = None
    version_json = ws.launcher_root / f"cache/minecraft/{args.mc}/version.json"
    if version_json.is_file():
        main_class = json.loads(version_json.read_text()).get("mainClass")
    java_reached = args_file.is_file() and (
        main_class is None or main_class in args_file.read_text().split()
    )

    if result.harness_error:
        reasons.append(f"harness: {result.harness_error}")
    if phase.expected_hosts is not None:
        if dict(hosts) != phase.expected_hosts:
            reasons.append(f"requests {dict(hosts)} != expected {phase.expected_hosts}")
    elif hosts.get(phase.must_see, 0) < 1:
        reasons.append(f"no request to {phase.must_see}: the proxy is not in the loop")
    if java_reached != phase.expect_java:
        reasons.append(
            "the launch reached java" if java_reached else "the launch never reached java"
        )
    if phase.proxy_on_closed_port and result.state != "launch_failed":
        reasons.append(f"state {result.state}, expected launch_failed with the proxy unreachable")
    for piece in phase.failure_must_contain:
        piece = piece.format(mc=args.mc)
        if piece not in (result.failure_summary or ""):
            reasons.append(f"the failure does not say {piece!r}")
    reasons += loader_cache_reasons(ws, entry, phase.loader_cache_after, "after the launch")

    return Outcome(
        name=name,
        passed=not reasons,
        hosts=hosts,
        java_reached=java_reached,
        state=result.state,
        failure=result.failure_summary,
        reasons=reasons,
        log_lines=log_path.read_text(encoding="utf-8").splitlines(),
        launch_log_dir=result.launch_log_dir,
    )


def run_scenario(
    args: argparse.Namespace, scenario: Scenario, display: str, display_env: dict
) -> list[Outcome]:
    ws = build_workspace(args, scenario)
    try:
        return [
            run_phase(args, ws, scenario, phase, display, display_env)
            for phase in scenario.phases
        ]
    finally:
        ws.cleanup()

# ---- display --------------------------------------------------------------------


def start_display(choice: str) -> tuple[str, dict[str, str], subprocess.Popen | None]:
    """Return (run_combo display, env additions, process to stop)."""
    if choice == "auto":
        choice = "broadway" if shutil.which("broadwayd") else "xvfb"
    if choice == "xvfb":
        return "xvfb", {"GDK_BACKEND": "x11"}, None
    if choice != "broadway":
        raise ValueError(f"unknown display {choice!r}")
    for number in range(17, 40):
        if not port_is_open(8080 + number):
            break
    else:
        raise RuntimeError("no free broadway display between :17 and :39")
    process = subprocess.Popen(
        ["broadwayd", f":{number}"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
    )
    for _ in range(50):
        if port_is_open(8080 + number):
            break
        time.sleep(0.1)
    else:
        process.terminate()
        raise RuntimeError(f"broadwayd :{number} did not start")
    return "native", {"GDK_BACKEND": "broadway", "BROADWAY_DISPLAY": f":{number}"}, process


# ---- binario --------------------------------------------------------------------


def stale_binary_reason(binary: Path) -> str | None:
    """Why `binary` cannot be the measurement, or None.

    Un binario più vecchio dei sorgenti misura un altro codice, e lo fa in
    silenzio: è già successo con questa guardia, che prendeva il binario release
    di tre settimane prima (`run_combo._resolve_binary` preferisce release).
    """
    if not binary.is_file():
        return f"no launcher binary at {binary}: build it with `cargo build --bin cubic_launcher`"
    sources = [
        path
        for path in (REPO_ROOT / "src-tauri/src").rglob("*")
        if path.is_file()
    ] + [REPO_ROOT / name for name in SOURCE_FILES if (REPO_ROOT / name).is_file()]
    newest = max(sources, key=lambda path: path.stat().st_mtime)
    if newest.stat().st_mtime > binary.stat().st_mtime:
        return f"{binary} is older than {newest}: rebuild it before measuring"
    return None


# ---- main -----------------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--real-data", type=Path, default=DEFAULT_REAL_DATA)
    parser.add_argument("--work-dir", type=Path, default=DEFAULT_WORK_DIR)
    parser.add_argument("--modlist", default=DEFAULT_MODLIST)
    parser.add_argument("--mc", default=DEFAULT_MC)
    parser.add_argument("--missing-jar", default=DEFAULT_MISSING_JAR, help="relative to cache/mods")
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY)
    parser.add_argument("--display", default="auto", choices=["auto", "broadway", "xvfb"])
    parser.add_argument("--timeout", type=int, default=150, help="timeoutSeconds per launch")
    parser.add_argument(
        "--only", action="append", default=None, help="run only these scenarios/controls"
    )
    parser.add_argument("--keep", action="store_true", help="keep the workspaces")
    args = parser.parse_args()

    stale = stale_binary_reason(args.binary)
    if stale:
        print(f"[guard] {stale}")
        return 1
    print(f"[guard] binary {args.binary}")
    if port_is_open(DEV_SERVER_PORT):
        print(
            f"[guard] something listens on 127.0.0.1:{DEV_SERVER_PORT}: the webview would load "
            "the frontend and its requests would end up in the log. Stop it first."
        )
        return 1
    args.work_dir.mkdir(parents=True, exist_ok=True)

    selected = [s for s in SCENARIOS + CONTROLS if args.only is None or s.name in args.only]
    display, display_env, display_process = start_display(args.display)
    print(f"[guard] display={args.display}->{display} {display_env}")
    verdicts: list[bool] = []
    try:
        for scenario in selected:
            print(f"[guard] {scenario.name} ...", flush=True)
            outcomes = run_scenario(args, scenario, display, display_env)
            verdicts.append(all(outcome.passed for outcome in outcomes))
            for outcome in outcomes:
                verdict = "PASS" if outcome.passed else "FAIL"
                counts = ", ".join(f"{n} {h}" for h, n in sorted(outcome.hosts.items())) or "0 requests"
                print(f"[guard] {verdict} {outcome.name}: {counts}; java reached={outcome.java_reached}; state={outcome.state}")
                for line in outcome.log_lines:
                    print(f"         proxy | {line}")
                if outcome.failure:
                    print(f"         failure | {outcome.failure[:300]}")
                for reason in outcome.reasons:
                    print(f"         why | {reason}")
    finally:
        if display_process is not None:
            display_process.terminate()
            display_process.wait(timeout=10)

    passed = all(verdicts)
    print(f"[guard] {'PASS' if passed else 'FAIL'}: {sum(verdicts)}/{len(verdicts)}")
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
