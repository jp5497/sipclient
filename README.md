# RustSIP — Softphone SIP Nativo Linux

Softphone SIP moderno, corporativo e ultrarrápido desenvolvido **100% em Rust** para Linux (otimizado para Arch Linux e distribuições modernas).

Projetado como uma alternativa nativa de alta performance e código aberto ao MicroSIP e Zoiper, sem Electron, sem WebViews e sem consumo desnecessário de memória.

---

## 🚀 Destaques e Funcionalidades

- **100% Rust Nativo:** Interface gráfica com aceleração por hardware via `egui`/`eframe`.
- **Áudio Nativo PipeWire / PulseAudio:** Integração direta com `libpulse-simple` FFI em 8000 Hz S16LE Mono para latência ultrabaixa (< 20ms) e estabilidade total.
- **Detecção Automática de Dispositivos:** Seleção de fones de ouvido (Bluetooth/USB) e microfones em tempo real sem travamentos.
- **Codecs de Voz Integrados:** Implementação em Rust puro de **G.711 µ-law (PCMU)** e **G.711 A-law (PCMA)**.
- **Sinalização SIP RFC 3261:**
  - Registro e autenticação Digest Auth MD5 com controle de `nonce` e `qop=auth`.
  - Suporte a servidores **FreeSWITCH**, **Asterisk**, **OpenSIPS** e **Kamailio**.
  - Conexão tanto via **UDP** quanto via **TCP**.
  - Suporte a Early Media (**183 Session Progress**) com transição fluida para atendimento (**200 OK**).
- **RTP & DTMF:**
  - Gerenciador RTP em sockets UDP dinâmicos com RFC 4733 (`telephone-event`) e SIP INFO.
  - Sintetizador de tons DTMF locais e toques de chamada (Ringback / Ringtone).
  - Travessia de NAT via cliente **STUN (RFC 5389)** integrado.
- **Interface Corporativa & Produtividade:**
  - Design sóbrio, moderno e focado em segurança e ergonomia.
  - Teclado numérico físico e virtual com resposta tátil.
  - Gerenciamento de chamadas: Atendimento, Recusa, Mudo (*Mute*) e Espera (*Hold*).
  - Agenda de Contatos local com busca em tempo real.
  - Histórico de chamadas com filtros (Efetuadas, Recebidas, Perdidas) e rediscagem rápida.
  - Inspetor técnico de sinalização SIP ao vivo com visualização de pacotes brutos.
  - Medidores VU de entrada/saída de áudio e telemetria de pacotes RTP.

---

## 🛠️ Requisitos e Instalação

### Dependências do Sistema

No **Arch Linux**:
```bash
sudo pacman -S libpulse base-devel git
```

No **Ubuntu / Debian**:
```bash
sudo apt update
sudo apt install libpulse-dev build-essential pkg-config
```

No **Fedora**:
```bash
sudo dnf install pulseaudio-libs-devel gcc
```

---

## 📦 Compilação e Execução

Clone o repositório e compile com o Cargo:

```bash
git clone https://github.com/jp5497/sipclient.git
cd sipclient
cargo build --release
```

O binário otimizado estará disponível em:
```bash
./target/release/rustsip
```

Para executar os testes automatizados da suíte:
```bash
cargo test
```

---

## ⚙️ Configuração

Ao abrir o RustSIP, acesse a aba **Conta**:
1. **Endereço do Servidor:** IP ou domínio do seu servidor FreeSWITCH / PBX (ex: `voipotimizar.otimizartecnologia.com.br`).
2. **Usuário / Ramal:** Seu número de ramal (ex: `2003`).
3. **Senha SIP:** Sua senha do ramal.
4. **Transporte:** Selecione **UDP** ou **TCP** conforme a porta configurada no seu servidor.
5. Clique em **Salvar e Conectar**. As credenciais e preferências de áudio são salvas de forma segura em `~/.config/rustsip/config.json`.

---

## 🖥️ Daemon + System Tray

A partir da refatoração daemon+IPC, o RustSIP roda em **dois papéis** sobre o
mesmo binário, para manter o registro SIP e a chamada vivos mesmo com a janela
fechada:

- **`rustsip`** (padrão): garante que o **daemon** esteja no ar e abre a
  **janela**. Se o daemon já estiver rodando, apenas conecta a ele.
- **`rustsip daemon`**: sobe só o **daemon headless** (núcleo SIP + áudio +
  ícone de bandeja), sem janela.

O daemon é o único dono do núcleo: ele registra no PBX, trata as chamadas e é o
único processo que escreve em `~/.config/rustsip/config.json`. A janela é um
**cliente** que fala com o daemon por um socket Unix local.

### Fechar a janela × Sair

- **Fechar a janela** (com bandeja disponível): a **janela encerra**, mas o
  **daemon continua em segundo plano** — o registro SIP e uma chamada em curso
  seguem vivos. Reabra pelo ícone da bandeja em **"Mostrar"**.
- **"Sair"** (no menu da bandeja): encerra o **daemon** de forma graciosa
  (desliga a chamada, faz un-REGISTER e para o áudio). É a única ação que
  realmente finaliza o app.
- **Fallback sem host SNI** (sem bandeja no ambiente): como não haveria ícone
  para reabrir, **fechar a janela também encerra o daemon** — o app não fica
  inacessível em background.

### Requisitos da bandeja

O ícone usa o padrão **StatusNotifierItem (SNI)** via D-Bus, suportado
nativamente no **KDE Plasma** e em outros ambientes Linux/BSD com host SNI.
O **GNOME puro** não expõe host SNI por padrão — é necessária a extensão
**AppIndicator and KStatusNotifierItem Support** para o ícone aparecer. Sem um
host SNI, o app cai no fallback descrito acima (fechar encerra o daemon).

### Arquivos em runtime

- **Log do daemon:** `~/.config/rustsip/daemon.log`
- **Socket IPC:** `$XDG_RUNTIME_DIR/rustsip/` (socket Unix local, só o dono
  conecta — permissão 0600).

---

## 📄 Licença

Distribuído sob a licença MIT. Consulte o arquivo [LICENSE](LICENSE) para obter mais informações.
