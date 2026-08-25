# r3 USB-C routing erratum

This note applies to the r3 schematic and boards assembled from it. It records
the definitive wiring of the stacked `XUBF-0336-24B02` connector and
connected-board failures observed on the PD/COMM receptacle.

## Definitive schematic connections

The connector symbol groups contacts by receptacle number, not by their
vertical placement in the drawing.

Receptacle A is labelled `USB PD/COMM SIDE`:

- `1A5 / CC1` connects to STUSB4500 `CC1`.
- `1B5 / CC2` connects to STUSB4500 `CC2`.
- Both plug orientations' D+/D- contacts connect to `USB_A_P` and `USB_A_N`.
- Mechanical switch S2 can connect `USB_A_P` and `USB_A_N` to the MCU USB data
  pair.

Receptacle B is labelled `USB COMM SIDE`:

- `2A5 / CC1` and `2B5 / CC2` are both unconnected (explicit no-connect marks).
  This receptacle has no USB-C sink termination and is not expected to work
  from a USB-C-to-USB-C host connection.
- **All four receptacle-B VBUS contacts (`2A4`, `2A9`, `2B4`, `2B9`) are also
  explicit no-connects.** Receptacle B is data-and-ground only; its VBUS is
  electrically isolated from the board.
- Both plug orientations' D+/D- contacts connect to `USB_B_P` and `USB_B_N`.
- S2 can connect `USB_B_P` and `USB_B_N` to the MCU USB data pair.
- A USB-A-to-USB-C cable can carry data because the MCU's USB device pull-up
  is driven from board power, so enumeration does not depend on host VBUS
  reaching the board.

Only receptacle A's VBUS contacts (`1A4`, `1A9`, `1B4`, `1B9`) connect to the
`VBUS` net. Q1 separates that connector-side VBUS from downstream
`VBUS_SINK`. S2 switches only D+ and D-.

> Correction (2026-08-25): an earlier revision of this note claimed all VBUS
> contacts of both receptacles shared one net, creating a supply-paralleling
> hazard when a powered COM cable and a PD source were connected
> simultaneously. Re-examination of the r3 schematic shows receptacle B's
> VBUS pins carry no-connect marks, so no such shared-VBUS path exists in the
> schematic and simultaneous use of a PD source on receptacle A with a
> USB-A COM cable on receptacle B is a supported arrangement. This matches
> operating experience. (Caveat: this is a schematic-level conclusion; it has
> not been continuity-verified on an assembled board.)

## Connected-board evidence

Receptacle A should support one-cable power and USB data in either plug
orientation when S2 selects `USB_A`. On the tested board, however:

- No S2 position or cable orientation produced USB CDC data when receptacle A
  was connected directly to a computer.
- With a standards-compliant PPS source connected to receptacle A, repeated
  `SYST:PD:RAW?` snapshots reported `PORT0x00 CC0x20 TYPEC0x01`, meaning the
  STUSB4500 saw neither source Rp and remained in `ATTACHWAIT_SNK`.
- The independent sink ADC measured about 19.37 V despite the STUSB4500 having
  no active RDO. VBUS presence is therefore not evidence of a live PD contract.

The schematic does not explain those failures as intended behavior: both CC
contacts and both USB 2.0 data orientations are routed for receptacle A. The
fault boundary is the assembled receptacle-A path: connector joints/footprint,
CC1 and CC2 continuity through D3 to the STUSB4500, USB_A D+/D- continuity
through S2, or the corresponding populated components.

## Safe connection modes

### One cable for power and data

The schematic-supported arrangement uses only receptacle A, with receptacle B
unconnected:

1. Connect receptacle A to a USB-C host or dock that supplies both USB data and
   power. A charger-only PPS brick cannot provide the USB host data connection
   required for CDC.
2. Put S2 in the `USB_A_P` / `USB_A_N` position.
3. Confirm USB CDC enumeration and query `SYST:PD:RAW?`.
4. Require Type-C attachment and a verified RDO before enabling outputs.

This arrangement is presently a diagnostic target, not a verified working
mode on the tested board. The available fixed PDOs and maximum power are
determined by the host or dock.

### Separate PPS source and COM connection

A powered USB-A-to-USB-C COM cable on receptacle B together with a separate PD
source on receptacle A is supported by the r3 schematic: receptacle B's VBUS
contacts are no-connects, so the two supplies never meet. This is the standard
bench arrangement for connected PD diagnostics.

## Required hardware checks and correction

### Non-invasive diagnosis when the board cannot be probed

The tested board is installed such that continuity probing and connector rework
are not practical. Use this cable-only test with the PPS source and receptacle-B
COM cable both disconnected:

1. Put S2 in the receptacle-A (`USB_A`) position.
2. Connect only receptacle A to a known USB-C computer port with a known
   data-capable USB-C cable.
3. Observe whether the display powers and whether a CDC device appears. Repeat
   after flipping the plug at the board, even though the schematic routes both
   orientations.

The diagnostic firmware boots directly to `USB PD Input`. After approximately
500 ms it shows measured sink VBUS and either a passive error such as `PD
ERR:DETACHED`/`PD ERR:BUS` or the verified PDO number, voltage, and current. It
does not transmit a PD request during this boot diagnostic.

The result separates the accessible fault boundary:

- No board power in either orientation means the USB-C host is not recognizing
  a sink attachment; the receptacle-A CC/STUSB path is the primary failure.
- Board power with no CDC device in either S2 position means attachment and
  VBUS are present but the receptacle-A `USB_A` data path is failing.
- CDC enumeration allows `SYST:PD:RAW?` to verify attach and contract state
  directly over the same cable.

No firmware change can make a USB-C host enable its port when the host cannot
see the physical Rd/CC attachment. Connected PPS diagnostics over USB CDC use
the receptacle-B COM path, which is safe alongside the PD source per the
corrected receptacle-B wiring above.

### Probe/rework procedure when the board becomes accessible

With all cables and power removed:

1. Verify continuity from receptacle-A `1A5/CC1` and `1B5/CC2` through D3 to
   STUSB4500 pins 2 and 4 respectively; check that neither path is shorted to
   ground.
2. Verify both receptacle-A D+/D- contact pairs reach `USB_A_P/N` and that S2
   connects that pair to MCU USB D+/D- in exactly one position.
3. Inspect the stacked connector footprint, solder joints, D3, and S2 against
   the populated part numbers.
4. Correct any assembly/footprint fault found. (Receptacle-B VBUS is already
   omitted in the r3 schematic; a later revision only needs to preserve that.)

Source: [`Schematics/USB_PowerSupply_r3.pdf`](../Schematics/USB_PowerSupply_r3.pdf).
