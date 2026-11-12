package sample;

import static org.junit.jupiter.api.Assertions.assertEquals;

import org.junit.jupiter.api.Test;

class DigitsTest {
    @Test
    void aDependencySetupDownloadedIsThere() {
        assertEquals("124", Digits.of(124));
    }
}
