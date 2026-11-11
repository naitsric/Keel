import pytest


def test_a_dependency_setup_installed_is_there():
    assert pytest.__version__ == "8.3.5"
