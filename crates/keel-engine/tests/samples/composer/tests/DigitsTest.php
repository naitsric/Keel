<?php

namespace Sample\Tests;

use PHPUnit\Framework\TestCase;
use Sample\Digits;

final class DigitsTest extends TestCase
{
    public function testADependencySetupInstalledIsThere(): void
    {
        $this->assertSame('124', Digits::of(124));
    }
}
