defmodule S3ProxyIntegrationTest do
  use ExUnit.Case
  doctest S3ProxyIntegration

  test "greets the world" do
    assert S3ProxyIntegration.hello() == :world
  end
end
