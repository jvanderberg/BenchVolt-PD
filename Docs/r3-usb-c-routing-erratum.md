# r3 USB-C routing and shared-VBUS erratum

This note applies to the r3 schematic and boards assembled from it. It records
the definitive wiring of the stacked `XUBF-0336-24B02` connector, the shared
VBUS hazard, and connected-board failures observed on the PD/COMM receptacle.

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

- `2A5 / CC1` and `2B5 / CC2` are both unconnected. This receptacle has no
  USB-C sink termination and is not expected to work from a USB-C-to-USB-C
  host connection.
- Both plug orientations' D+/D- contacts connect to `USB_B_P` and `USB_B_N`.
- S2 can connect `USB_B_P` and `USB_B_N` to the MCU USB data pair.
- A USB-A-to-USB-C cable can carry data because a legacy USB-A source supplies
  VBUS without relying on USB-C CC attachment.

All VBUS contacts of both receptacles connect to the same `VBUS` net. There is
no power isolation between receptacles A and B. Q1 separates the shared
connector-side VBUS from downstream `VBUS_SINK`; it does not isolate the two
receptacles. S2 switches only D+ and D-.

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

Do not connect an ordinary powered USB-A-to-USB-C COM cable to receptacle B
while a separate PD source powers receptacle A. Both sources would connect to
the same VBUS net. The COM path needs a purpose-built VBUS blocker/data-only
adapter that retains D+, D-, and ground. Without one, disconnect the USB-A COM
cable before connecting the PPS source.

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

The result separates the accessible fault boundary:

- No board power in either orientation means the USB-C host is not recognizing
  a sink attachment; the receptacle-A CC/STUSB path is the primary failure.
- Board power with no CDC device in either S2 position means attachment and
  VBUS are present but the receptacle-A `USB_A` data path is failing.
- CDC enumeration allows `SYST:PD:RAW?` to verify attach and contract state
  directly over the same cable.

No firmware change can make a USB-C host enable its port when the host cannot
see the physical Rd/CC attachment. Until the one-cable mode enumerates or a
VBUS-blocking COM adapter is available, connected PPS diagnostics cannot be
performed safely over USB CDC on this board revision.

### Probe/rework procedure when the board becomes accessible

With all cables and power removed:

1. Verify continuity from receptacle-A `1A5/CC1` and `1B5/CC2` through D3 to
   STUSB4500 pins 2 and 4 respectively; check that neither path is shorted to
   ground.
2. Verify both receptacle-A D+/D- contact pairs reach `USB_A_P/N` and that S2
   connects that pair to MCU USB D+/D- in exactly one position.
3. Inspect the stacked connector footprint, solder joints, D3, and S2 against
   the populated part numbers.
4. Correct any assembly/footprint fault found. A later board revision must also
   isolate or intentionally omit receptacle-B VBUS so a COM cable cannot place
   a second external source on the PD VBUS net.

Source: [`Schematics/USB_PowerSupply_r3.pdf`](../Schematics/USB_PowerSupply_r3.pdf).
