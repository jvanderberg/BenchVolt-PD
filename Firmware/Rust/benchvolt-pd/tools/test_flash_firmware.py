import struct
import unittest

import flash_firmware


def image_of_size(size: int, reset_offset: int = 192) -> bytearray:
    image = bytearray([0xFF] * size)
    struct.pack_into(
        "<II",
        image,
        0,
        0x2000_4000,
        (flash_firmware.APP_ORIGIN + reset_offset) | 1,
    )
    return image


class ValidateImageTests(unittest.TestCase):
    def test_exact_92_kib_partition_is_accepted(self):
        capacity = flash_firmware.SETTINGS_ORIGIN - flash_firmware.APP_ORIGIN
        flash_firmware.validate_image(image_of_size(capacity))

    def test_one_byte_past_partition_is_rejected(self):
        capacity = flash_firmware.SETTINGS_ORIGIN - flash_firmware.APP_ORIGIN
        with self.assertRaisesRegex(ValueError, "overlapping settings"):
            flash_firmware.validate_image(image_of_size(capacity + 1))

    def test_vector_table_minimum_is_enforced(self):
        with self.assertRaisesRegex(ValueError, "192-byte vector table"):
            flash_firmware.validate_image(bytes(191))

    def test_reset_vector_must_be_thumb_and_inside_image(self):
        image = image_of_size(256)
        struct.pack_into("<I", image, 4, flash_firmware.APP_ORIGIN + 192)
        with self.assertRaisesRegex(ValueError, "not a Thumb address"):
            flash_firmware.validate_image(image)

        struct.pack_into("<I", image, 4, (flash_firmware.APP_ORIGIN + 256) | 1)
        with self.assertRaisesRegex(ValueError, "outside the image"):
            flash_firmware.validate_image(image)


if __name__ == "__main__":
    unittest.main()
