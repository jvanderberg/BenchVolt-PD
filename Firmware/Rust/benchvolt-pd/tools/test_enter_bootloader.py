import unittest

import enter_bootloader


class OutputSafetyTests(unittest.TestCase):
    def test_accepts_exactly_seven_inactive_output_fields(self):
        fields = [b"0"] * 27
        enter_bootloader.require_outputs_off(b",".join(fields))

    def test_rejects_each_active_output_or_arb_field(self):
        for index in range(13, 20):
            fields = [b"0"] * 27
            fields[index] = b"1"
            with self.subTest(index=index), self.assertRaises(RuntimeError):
                enter_bootloader.require_outputs_off(b",".join(fields))

    def test_rejects_an_unexpected_protocol_shape(self):
        with self.assertRaises(RuntimeError):
            enter_bootloader.require_outputs_off(b"0,0,0")


if __name__ == "__main__":
    unittest.main()
