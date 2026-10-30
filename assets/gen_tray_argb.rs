// Gerador one-shot do assets/tray-64.argb.
//
// Reaproveita a LOGICA de build_icon_rgba da branch feat/system-tray: glifo de
// telefone (dois discos nas pontas de uma diagonal + haste espessa), desenhado
// em BRANCO opaco com CONTORNO PRETO de 1px ao redor de cada pixel aceso
// (contraste dual: visivel tanto em painel claro quanto escuro), fundo
// transparente. O original desenha num grid logico 22x22; aqui amostramos esse
// mesmo grid para 64x64 (nearest) e emitimos ARGB32 em network byte order
// (big-endian: [A,R,G,B] por pixel), que e o formato exigido por ksni::Icon.
//
// NAO entra no build do crate; e so o produtor dos bytes versionados.
const SRC: i32 = 22;
const DST: i32 = 64;

fn build_mask_22() -> Vec<bool> {
    let w = SRC as usize;
    let h = SRC as usize;
    let mut mask = vec![false; w * h];
    let mut set = |m: &mut [bool], x: i32, y: i32| {
        if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
            m[y as usize * w + x as usize] = true;
        }
    };
    for (cx, cy) in [(6i32, 6i32), (15, 15)] {
        for dy in -2..=2i32 {
            for dx in -2..=2i32 {
                if dx * dx + dy * dy <= 5 {
                    set(&mut mask, cx + dx, cy + dy);
                }
            }
        }
    }
    for t in 0..=7 {
        let (x, y) = (7 + t, 7 + t);
        set(&mut mask, x, y);
        set(&mut mask, x + 1, y);
        set(&mut mask, x, y + 1);
    }
    mask
}

fn main() {
    let mask = build_mask_22();
    let lit_src = |x: i32, y: i32| -> bool {
        x >= 0 && y >= 0 && x < SRC && y < SRC && mask[(y * SRC + x) as usize]
    };
    let lit_dst = |x: i32, y: i32| -> bool {
        if x < 0 || y < 0 || x >= DST || y >= DST {
            return false;
        }
        let sx = x * SRC / DST;
        let sy = y * SRC / DST;
        lit_src(sx, sy)
    };
    let mut argb = vec![0u8; (DST * DST * 4) as usize];
    for y in 0..DST {
        for x in 0..DST {
            let idx = ((y * DST + x) * 4) as usize;
            if lit_dst(x, y) {
                argb[idx..idx + 4].copy_from_slice(&[255, 255, 255, 255]);
            } else {
                let border = (-1..=1).any(|dy: i32| {
                    (-1..=1).any(|dx: i32| (dx, dy) != (0, 0) && lit_dst(x + dx, y + dy))
                });
                if border {
                    argb[idx..idx + 4].copy_from_slice(&[255, 0, 0, 0]);
                }
            }
        }
    }
    let alpha_px = argb.chunks(4).filter(|p| p[0] == 255).count();
    eprintln!("argb len={} alpha255_px={}", argb.len(), alpha_px);
    assert!(alpha_px > 0, "glifo vazio");
    std::fs::write("assets/tray-64.argb", &argb).unwrap();
    eprintln!("gravado assets/tray-64.argb");
}
