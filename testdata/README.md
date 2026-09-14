These certificates and the private key are public test fixtures, not application
credentials. They are valid from 2020-01-01 through 2045-01-01 for download.test
and cdn.test only. Never use them to protect a real deployment. The CA is trusted
only by a temporary Go x509.CertPool inside tests; it is not installed system-wide.

Run the Go test suite with `go test ./...`. Certificate generation is not part
of the test run or application startup.
