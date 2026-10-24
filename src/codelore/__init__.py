"""CodeLore - project memory for coding agents, kept in the repository."""

__version__ = "0.1.2"
FORMAT_VERSION = 1


class LoreError(Exception):
    """A refusal: something the user or the data must change. Said by name."""

    def __init__(self, message: str, details: list[str] | None = None):
        super().__init__(message)
        self.details = details or []

    def __str__(self) -> str:
        head = super().__str__()
        if not self.details:
            return head
        return head + "\n" + "\n".join(f"  - {d}" for d in self.details)
