package io.github.aaif_kaji.providers.openai

public fun provider(apiKey: String): io.github.aaif_kaji.Provider = io.github.aaif_kaji.openaiProvider(apiKey)

public fun defaultModel(): String = io.github.aaif_kaji.openaiDefaultModel()
