# BenchVolt PD Hardware BOM and Cost Estimate

This bill of materials was transcribed by hand from the upstream schematic,
[`Schematics/USB_PowerSupply_r3.pdf`](../Schematics/USB_PowerSupply_r3.pdf)
(revision r3). The upstream project does not publish a BOM file, so treat this
as a reconstruction, not an official parts list.

> **Prices are approximate.** Unit prices are LCSC/JLCPCB list prices checked on
> 2026-09-27 at the 100-piece price break. They do not include larger-volume
> discounts (1k+ reels, negotiated or distributor pricing), and they will
> drift over time. Western distributors such as DigiKey and Mouser are
> typically 1.3–2× higher for the ICs. Items marked **E** are estimates, not
> looked-up prices. Shipping, duties, tariffs, labor, and margin are not
> included.

## Architecture notes

- **MCU:** STM32F070RBT6 (Cortex-M0, 128 KiB flash, 16 KiB RAM).
- **USB PD:** a dedicated STUSB4500 sink controller negotiates the input
  contract. The STM32 supervises it over I²C but has no PD hardware of its own.
- **Power stages:** the buck-boost sheet (`BuckBoost.SchDoc`) is instantiated
  three times, so parts on that sheet are marked **×3**. The schematic shows only
  one set of reference designators for it.
  - **DC1** (TPS55289, set to 3.0 V by firmware) feeds the 1.8 V and 2.5 V LDOs.
  - **DC2** (TPS55289, set to 5.5 V) feeds the 3.3 V LDO and the adjustable
    0.5–5.5 V LDO.
  - **CH5** (TPS55289) runs directly from the USB input and provides 0.8–22 V.
- **Display:** ST7789 controller driving a 170×320 IPS panel (generic 1.9″ SPI
  module on an 8-pin 2.54 mm header).

## ICs and semiconductors

| Qty | Ref | Part | Function | Unit @100 | Ext | Src |
|---|---|---|---|---|---|---|
| 1 | B11 | [STM32F070RBT6](https://www.st.com/resource/en/datasheet/stm32f070rb.pdf) | MCU | $1.28 | $1.28 | L |
| 1 | B2 | [STUSB4500QTR](https://www.st.com/resource/en/datasheet/stusb4500.pdf) | USB-C PD sink controller | $1.78 | $1.78 | L |
| 3 | U5 ×3 | [TPS55289RYQR](https://www.ti.com/lit/ds/symlink/tps55289.pdf) | Buck-boost (DC1, DC2, CH5) | $3.94 | $11.82 | L |
| 3 | B3, B4, B5 | [LM29302R (HTC Korea)](https://www.htckorea.co.kr/Datasheet/VLDO/LM2930x.pdf) | 3.3 / 2.5 / 1.8 V LDOs | $0.72 | $2.16 | L |
| 1 | U1 | [MIC69502WR](https://ww1.microchip.com/downloads/aemDocuments/documents/APID/ProductDocuments/DataSheets/MIC69502-5A-Low-VIN-Low-VOUT-%CE%BCCap-LDO-Regulator-DS20006836.pdf) | 0.5–5.5 V adjustable LDO | $3.60 | $3.60 | L |
| 1 | B1 | [MCP4725A0T-E/CH](https://ww1.microchip.com/downloads/en/devicedoc/22039d.pdf) | DAC that sets the adjustable LDO | $0.64 | $0.64 | L |
| 1 | B7 | [AP64060WU-7](https://www.diodes.com/datasheet/download/AP64060_AP64062.pdf) | USB → 3.8 V buck | $0.35 | $0.35 | L |
| 1 | U2 | [TLV77533PDBVR](https://www.ti.com/lit/ds/symlink/tlv775.pdf) | 3.3 V LDO for the MCU | $0.07 | $0.07 | L |
| 6 | B6, B9, B12–B15 | [INA180A2IDBVR](https://www.ti.com/lit/ds/symlink/ina180.pdf) | Current-sense amplifiers | $0.15 | $0.90 | L |
| 1 | B10 | [TMP1075DGKR](https://www.ti.com/lit/ds/symlink/tmp1075.pdf) | Temperature sensor | $0.26 | $0.26 | L |
| 1 | Q1 | [STL6P3LLH6](https://www.st.com/resource/en/datasheet/stl6p3llh6.pdf) | P-FET that switches VBUS | $0.57 | $0.57 | L |
| 4 | D1, D4, D6, D7 | [P4SMAJ6.0ADF-13](https://www.diodes.com/datasheet/download/P4SMAJ5.0ADF-P4SMAJ85ADF.pdf) | TVS diodes, LDO rails | $0.24 | $0.96 | L |
| 1 | D9 | [P4SMAJ24ADF-13](https://www.diodes.com/datasheet/download/P4SMAJ5.0ADF-P4SMAJ85ADF.pdf) | TVS diode, 0.8–22 V output | $0.26 | $0.26 | L |
| 2 | D2, D8 | [ESDA25P35-1U1M](https://www.st.com/resource/en/datasheet/esda25p35-1u1m.pdf) | ESD protection (VBUS, NRST) | $0.15 | $0.30 | L |
| 2 | D3, D5 | [ESDA25W](https://www.st.com/resource/en/datasheet/esdaxxxwx.pdf) | ESD protection (CC, USB D±) | $0.11 | $0.22 | L |
| 6 | LD1–LD3, LD4 ×3 | 0603 LEDs | Status and fault indicators | $0.01 | $0.06 | L |
| | | | **Subtotal** | | **$25.23** | |

The printed part "LM29302R" is HTC Korea / Taejin's 3 A adjustable LDO, a
pin-compatible second source of Microchip's
[MIC29302](http://ww1.microchip.com/downloads/en/DeviceDoc/MIC2915x-30x-50x-75x-High-Current-Low-Dropout-Regulators-DS20005685B.pdf),
which works as a drop-in alternative.

## Passives

| Qty | Ref | Part | Value | Unit @100 | Ext | Src |
|---|---|---|---|---|---|---|
| 3 | L1 ×3 | [FXL1360-4R7-M](https://www.lcsc.com/datasheet/C526021.pdf) | 4.7 µH | $0.34 | $1.02 | L |
| 1 | L2 | [TMS322512ALM-100MTAA](https://product.tdk.com/system/files/dam/doc/product/inductor/inductor/smd/catalog/inductor_commercial_power_tms322512alm_en.pdf) | 10 µH | $0.26 | $0.26 | L |
| 1 | FB1 | FBMH1608HM221-T | Ferrite bead | $0.037 | $0.04 | L |
| 1 | X1 | [NX3225GD-8MHZ-STD-CRA-3](https://www.lcsc.com/datasheet/C889706.pdf) | 8 MHz crystal | $0.27 | $0.27 | L |
| 3 | C4 ×3 | Panasonic EEHZA1H680P | 68 µF electrolytic | $0.35 | $1.05 | L |
| 3 | C34 ×3 | Panasonic EEHZK1V101XP | 100 µF electrolytic | $0.56 | $1.68 | L |
| 3 | C9, C15, C21 | AVX TCJB106M035R0200 | 10 µF 35 V tantalum¹ | $0.75 | $2.25 | L |
| 24 | C8, C13, C14, C20, C22, C23, C47, C52, C54, (C5, C10, C31–C33) ×3 | Samsung CL31B106KBHNNNE | 10 µF 50 V 1206 | $0.28 | $6.72 | L |
| 23 | C1, C6, C7, C24, C27, C37, C40, C42, C44, C46, C48, C49, C51, C53, C55–C63 | Murata GCJ188R71H104KA12D | 0.1 µF 50 V 0603 | $0.046 | $1.06 | L |
| 24 | (C3, C12, C16, C19, C28, C29, C36, C43) ×3 | Murata GRM155R71H104ME14D | 0.1 µF 50 V 0402 | $0.010 | $0.24 | L |
| 12 | C17, C18, C50, (C11, C30, C35) ×3 | Murata GRT188R61H105ME13D | 1 µF 50 V 0603 | $0.066 | $0.79 | L |
| 1 | C45 | Murata GCM31CR71H225KA55L | 2.2 µF 50 V 1206 | ² | ² | L |
| 3 | C2 ×3 | Murata GRT188R61C475KE13D | 4.7 µF 16 V 0603 | ² | ² | L |
| 3 | C41 ×3 | TDK CGA2B3X7R1H103K050BB | 10 nF 50 V 0402 | ² | ² | L |
| 3 | C39 ×3 | TDK CGA2B2X7R1H472K050BA | 4.7 nF 50 V 0402 | ² | ² | L |
| 3 | C38 ×3 | TDK CGA2B2C0G1H101J050BA | 100 pF 50 V 0402 | ² | ² | L |
| 2 | C25, C26 | FH 0603CG8R0C500NT | 8 pF 50 V 0603 | ² | $0.56² | L |
| 9 | R2, R4, R9, R15, R22, R31, R20 ×3 | Current shunt | 10 mΩ | $0.06 | $0.54 | E |
| 76 | All other resistors | 0603 1% thick film | See below | $0.003 | $0.23 | L/E |
| | | | **Subtotal** | | **$16.70** | |

¹ Unlabeled polarized capacitors on the schematic. Value inferred from the
schematic's part notes.
² These six small-capacitor lines were priced together at $0.56 per board.

**Resistor values:** 10 Ω ×6, 100 Ω ×4, 1 kΩ ×19, 1.1 kΩ, 2.2 kΩ ×4,
4.7 kΩ ×6 (I²C pull-ups), 5.36 kΩ, 5.76 kΩ ×2, 6.04 kΩ, 6.8 kΩ ×3, 6.98 kΩ,
9.31 kΩ, 10 kΩ ×6, 10.7 kΩ, 11.5 kΩ, 22 kΩ, 22.1 kΩ, 24.3 kΩ, 27.4 kΩ ×3,
49.9 kΩ ×3, 84.5 kΩ, 100 kΩ ×4, 150 kΩ ×3, 0 Ω ×2. R67 and R71 are not
fitted. The schematic does not specify resistor package or tolerance.

## Connectors and mechanical

| Qty | Ref | Part | Unit @100 | Ext | Src |
|---|---|---|---|---|---|
| 1 | J1 | [XUBF-0336-24B02](https://www.lcsc.com/datasheet/C7529553.pdf) USB-C (5 A / 48 V) | $2.50 | $2.50 | L |
| 1 | S2 | [XKB8080-Z](https://www.lcsc.com/datasheet/C318860.pdf) USB-side select switch | $0.14 | $0.14 | L |
| 1 | S1 | [TS-1145A-D-B](https://www.lcsc.com/datasheet/C18198078.pdf) reset switch | $0.10 | $0.10 | L |
| 1 set | J2–J7 | Pin headers (SWD 1×7, 2×13, 1×5 encoder, 1×8 display, 2× 2×2 LDO bypass) + 2 jumper caps | — | $0.37 | L/E |
| 6 | — | 4 mm banana jacks | $0.40 | $2.40 | E |
| 6 | MH1–MH6 | M3 standoffs and screws | $0.05 | $0.30 | E |
| | | **Subtotal** | | **$5.81** | |

## Display and PCB

| Qty | Part | Unit @100 | Ext | Src |
|---|---|---|---|---|
| 1 | ST7789 1.9″ 170×320 IPS SPI module, 8-pin ([ST7789V datasheet](https://newhavendisplay.com/content/datasheets/ST7789V.pdf); [similar module: Adafruit #5394](https://cdn-learn.adafruit.com/downloads/pdf/adafruit-1-9-color-ips-tft-display.pdf)) | $2.00 | $2.00 | E |
| 1 | 4-layer PCB, about 150×70 mm (JLCPCB-class) | $2.50 | $2.50 | E |
| | **Subtotal** | | **$4.50** | |

## Cost summary

| | Per unit |
|---|---|
| ICs and semiconductors | $25.23 |
| Passives | $16.70 |
| Connectors and mechanical | $5.81 |
| Display and PCB | $4.50 |
| **Board and parts** | **$52.24** |
| Assembly (JLCPCB PCBA) | $3–5 (E) |
| Enclosure (5 laser-cut acrylic panels + machined aluminium base) | $12–20 (E) |
| Rotary encoder and knob | $1–2 (E) |
| **Complete unit (approximate)** | **~$70–80** |

These figures use 100-piece pricing and are an upper bound for high-volume
production. At 1k+ quantities the per-unit cost would be lower.

Main cost drivers are the three TPS55289 buck-boosts (~$12), the twenty-four
10 µF/50 V 1206 MLCCs (~$7), the MIC69502 LDO ($3.60), and the USB-C connector
($2.50). LCSC stock was thin for the STL6P3LLH6, EEHZA1H680P, MIC69502WR, and
LM29302R when these prices were checked.

## Datasheets

Part numbers in the tables above link to English-language datasheets,
collected 2026-09-27. Manufacturer PDFs are used where they exist; the
FXL1360 inductor, the exact NX3225GD crystal spec, the USB-C connector
(mechanical drawing only), and the two switches link to LCSC-hosted copies.
The ST and TDK links are the manufacturers' own paths, but those sites blocked
the automated check, so they were not opened. The display module has no official
spec sheet. The linked Adafruit guide covers a similar ST7789 170×320 module.
