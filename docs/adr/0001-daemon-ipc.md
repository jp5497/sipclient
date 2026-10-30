# ADR-0001: RustSIP em daemon + processo-janela com IPC por Unix socket

**Status:** aprovado e implementado (branch feat/daemon-ipc-f0, fases F0-F4)

## Contexto

O RustSIP é um softphone SIP em Rust com GUI eframe/egui (processo único). O SIP (registro, chamadas, RTP) roda em thread Tokio própria do SipController; a UI lê estado in-process via Mutex/atômicos e envia SipCommand por mpsc. Requisito: o SIP deve permanecer REGISTRADO e com chamadas ATIVAS mesmo com a janela fechada (system tray). No Wayland, eframe/egui NÃO consegue ocultar nem recriar a janela em processo único (set_visible é no-op), então "minimizar para a bandeja" em processo único é inviável. A solução (inspirada em jp5497/galaxybudsmanager) é separar em DAEMON headless (dono do SIP/áudio/tray) + processo-janela, comunicando por IPC sobre Unix socket. Tray via crate ksni direto (D-Bus/StatusNotifierItem), sem GTK.

## Decisão

### Topologia
Um binário `rustsip`, papel escolhido por std::env::args_os():
- `rustsip` (default): garante que o daemon está vivo (sobe se preciso), depois roda a janela no próprio processo, como cliente IPC.
- `rustsip daemon`: headless -- AudioEngine + SipController + servidor IPC + ksni + dono do config. Sem eframe.
Quando o daemon relança a janela (tray "Mostrar" ou chamada entrando), executa `rustsip` default. Dois papéis, não três.

Donos: daemon é dono de SipController/SipSharedState/RtpStats, AudioEngine/AudioSharedState, TrayService ksni, config.json (ÚNICO writer) e histórico. A janela tem só um espelho do estado e o estado de UI (aba, filtro, toast, rascunhos). O AudioEngine da janela é inert (não abre PipeWire). Listagem de devices é feita na janela via query Pulse read-only.

Início do daemon (lazy): `rustsip` tenta conectar ao socket; se conecta, vai para a janela; senão faz Command::new(current_exe()).arg("daemon") com process_group(0), stdin null, stdout/stderr em ~/.config/rustsip/daemon.log, e retry de connect a cada 100ms por 3s. Instância única via std::fs::File::try_lock() em $XDG_RUNTIME_DIR/rustsip/daemon.lock (sem libc); o lockfile NÃO é apagado (evita corrida de inode do flock). Daemon ignora SIGHUP; SIGTERM/SIGINT = "Sair" (handlers instalados ANTES do auto_connect).

Encerramento do daemon: só pelo "Sair" do tray, SIGTERM/SIGINT, ou fallback sem tray. Sequência: broadcast Shutdown às janelas; espera client_count==0 (teto ~1s) para o Shutdown chegar antes do exit; Hangup (BYE) se em chamada + Disconnect (un-REGISTER sempre que estado != Disconnected, espera só Disconnected, timeout 2s); stop_audio; unlink(sock); exit. O activate do ksni NÃO chama process::exit -- manda DaemonEvt ao core.

### Contrato IPC
- Caminho: $XDG_RUNTIME_DIR/rustsip/daemon.sock (dir 0700, socket chmod 0600). Fallback ~/.config/rustsip/ se XDG_RUNTIME_DIR ausente. Defesa: peer_cred().uid() deve ser igual ao uid do processo (lido de /proc/self, sem libc), senão fecha (falha fechada: uid ilegível => u32::MAX => não casa).
- Framing: NDJSON (um serde_json por linha, \n). Teto por linha: 256 KiB (leitura com .take()); linha acima derruba a conexão. PROTO_VERSION: u32 = 1.
- SEGURANÇA: o erro de desserialização de frame é sanitizado (classify()+line+column+len, NUNCA o Display do serde, que citaria o valor de um campo -- ex. a senha).

Janela -> daemon (ClientMsg, enum serde tag="t"): Hello; Connect; Disconnect; Dial{target}; Answer; Hangup; KeypadDigit(char) [daemon faz play_keypad_dtmf + SendDtmf]; SetMuted/SetHold(bool); SetMicGain/SetSpeakerVolume(f32); SetInputDevice/SetOutputDevice; StartLoopbackTest/StopLoopbackTest; SaveAccount{account,reconnect}; AddContact/DeleteContact(por id); ClearHistory; Quit; RequestShow.

Daemon -> janela (ServerMsg): Welcome{...} (snapshot 1x, SEM contacts/history -- essas vêm em frames Contacts/History separados logo após, para não estourar o teto); LogAppend{seq,entry}; Reg; Call(CallPhaseWire, InCall usa elapsed_ms não Instant); Flags; PublicAddr; AudioCfg/Contacts/History/Account POR SEÇÃO (nunca o AppConfig inteiro); Hot{mic,spk,tx,rx} (<=20/s, só se mudou); Raise; Shutdown.

Modelo: push de deltas com DIFF no daemon (Publisher, tick 50ms, lê os Mutex/atômicos in-process, broadcast só do que mudou). Única mudança em sip.rs: log_seq AtomicU64 incrementado DENTRO do lock de logs (coerência). Backpressure: mpsc(256) por cliente; Hot e baixa-freq usam try_send; Full desconecta o cliente lento (não trava o Publisher); timeout no handshake (Hello/Welcome). VU/RTP a 20Hz (~1,4 KB/s); PCM nunca atravessa o socket.

### Áudio no daemon
AudioEngine + threads PulseAudio ficam no daemon, intactos. Os VU meters e contadores RTP que a janela mostra vêm por IPC (Hot, 20Hz). No boot do daemon, devices de áudio salvos que não existem mais são zerados (evita falha de pa_simple_new -> chamada muda).

### Tray (ksni direto, no daemon)
Dep: ksni = "0.3" (async sobre tokio). cargo tree confirma: só zbus, nenhum gtk/gdk/glib. icon_pixmap: ARGB32 64x64 pré-renderizado (assets/tray-64.argb), glifo de telefone com contraste dual (não some em painel claro/escuro). activate (clique esq) = Mostrar; menu "Mostrar RustSIP" + "Sair"; tooltip com reg/call. "Mostrar": se há janela conectada -> Raise; senão relança a janela (guarda anti duplo-clique ~5s; gui.lock garante janela única). Re-anúncio automático quando o StatusNotifierWatcher reaparece (plasmashell reiniciado). SEM host SNI: o spawn() do ksni falha honestamente (ao contrário do assume_sni_available do tray-icon); Welcome.tray_available=false e fechar a janela passa a ENCERRAR o daemon (ClientMsg::Quit), para o app nunca ficar inacessível. NOTA: tray_available tem default=true antes do Welcome chegar -- janela de milissegundos, aceitável.

### Ciclo de vida da janela
Fechar = run_native retorna, processo-janela sai, socket fecha. COM tray: o daemon segue com SIP+chamada vivos (nada no daemon espera a janela). SEM tray: fechar encerra o daemon. Reabrir (tray ou relançar `rustsip`) = Hello, Welcome + backlog de logs, repinta o estado atual com elapsed_ms. Não manda Connect. Chamada entrando com a janela fechada: o daemon detecta a borda para Incoming e relança a janela (auto-show). Raise no Wayland pode ser barrado pelo anti-foco do KWin (mitigado com RequestUserAttention) -- limite de plataforma. Daemon caído com a janela aberta: banner "serviço parado" + reconnect 1s + respawn automático UMA vez por vida do processo-janela (não por queda); se desistir, o banner orienta reabrir.

## Fases entregues
- F0: daemon headless + ksni (default inalterado); lock de instância; Sair gracioso; drena histórico.
- F1: src/ipc.rs (protocolo+framing) + IpcServer + Publisher; janela lendo estado via socket.
- F2: default vira cliente IPC; comandos ClientMsg; config writer único no daemon; AudioEngine inert.
- F3: tray Mostrar relança/foca; Shutdown; fallback sem SNI; auto-show em Incoming; banner de daemon caído.
- F4: testes do apply_command, save assíncrono, README, sanitização do erro de desserialização (não vaza senha).

## Segurança
Socket em $XDG_RUNTIME_DIR 0700 + 0600 + check de peer uid. Senha trafega em claro no Welcome/SaveAccount (o mesmo uid já decifra o config.json; frame IPC nunca é logado; erro de desserialização sanitizado). Debug de SipAccountConfig é redigido. Recomendação operacional: rotacionar a senha SIP se o config.json tiver existido em 0644 antes da cifra local (feature anterior).

## Consequências
Fechar a janela não encerra mais o SIP quando há bandeja; "Sair" no tray encerra tudo. Config passa a ter writer único (daemon). Introduz 1 dependência (ksni) e 2 runtimes tokio no daemon. A UI lê um espelho do estado via socket (latência <=50ms, imperceptível). O E2E ao vivo (daemon + janela + PBX + KDE) é verificação manual do usuário. Alternativa descartada: IPC por D-Bus (zbus já presente via ksni) -- VU a 20Hz no barramento de sessão não é o uso adequado.
