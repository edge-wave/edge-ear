"""A recording says why it ended in a word an application can compare.

The reason arrives as a string, and every one of them is named on
``edge_ear.EndReason``. Reaching for it there rather than writing the
literal means a misspelling fails at the attribute, where it is
obvious, instead of by silently never matching.
"""

import edge_ear

NAMES = ["SILENCE", "MAX_LENGTH", "NO_SPEECH", "STOPPED"]


def test_every_ending_is_named_once():
    seen = set()
    for name in NAMES:
        value = getattr(edge_ear.EndReason, name)
        assert isinstance(value, str), name
        assert value not in seen, f"{name} names an ending already named"
        seen.add(value)


def test_the_names_are_one_word_each():
    for name in NAMES:
        value = getattr(edge_ear.EndReason, name)
        assert " " not in value, f"{value!r} is a sentence, not a name"
        assert value == value.lower(), value


def test_the_name_matches_the_attribute_it_sits_under():
    assert edge_ear.EndReason.SILENCE == "silence"
    assert edge_ear.EndReason.MAX_LENGTH == "max_length"
    assert edge_ear.EndReason.NO_SPEECH == "no_speech"
    assert edge_ear.EndReason.STOPPED == "stopped"
