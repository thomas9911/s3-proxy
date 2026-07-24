defmodule S3ProxyIntegration do
  @moduledoc """
  Documentation for `S3ProxyIntegration`.
  """

  def test do
    # ExAws.S3.list_buckets() |> ExAws.request!()

    ExAws.S3.put_bucket("asdf", "us-west-2") |> ExAws.request!()
  end
end
