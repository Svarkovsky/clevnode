# TODO / Future Roadmap

This file outlines the upcoming development tasks, porting plans, and optimization targets for the `clevnode` stack and `leviculum` core.

## Multi-Architecture Support (Cross-Compilation Goals)

The primary goal is to expand the project to support other popular embedded and server architectures, enabling deployment across a wider range of hardware:

- **MIPSEL (Little-Endian MIPS):**
  - **Target:** `mipsel-unknown-linux-musl`
  - **Hardware:** MediaTek/Ralink SoCs (MT7620, MT7621, MT7628) commonly found in devices like Xiaomi, Keenetic, and TP-Link routers.
  
- **ARMv7-A (32-bit ARM):**
  - **Target:** `armv7-unknown-linux-musleabihf`
  - **Hardware:** Older Raspberry Pi boards, Cortex-A7 based home routers, and various BeagleBone/Orange Pi clones.

- **ARM64 / AArch64 (64-bit ARM):**
  - **Target:** `aarch64-unknown-linux-musl`
  - **Hardware:** Modern Raspberry Pi (3/4/5), Cortex-A53/A72 based high-end routers, and low-power ARM servers.

- **x86_64 (64-bit Intel/AMD):**
  - **Target:** `x86_64-unknown-linux-musl`
  - **Hardware:** Standard servers, home PCs, VPS instances, and development environments.
