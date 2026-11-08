"""Share the real service lifecycle fixture with HA contracts."""

from tests_ha.conftest import ha_protocol as ha_protocol
from tests_ha.conftest import rust_backend as rust_backend
from tests_ha.reader_fixture import fictional_reader as fictional_reader
