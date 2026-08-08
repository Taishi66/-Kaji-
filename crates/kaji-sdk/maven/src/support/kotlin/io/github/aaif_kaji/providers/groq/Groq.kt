package io.github.aaif_kaji.providers.groq

public fun provider(apiKey: String): io.github.aaif_kaji.Provider = io.github.aaif_kaji.groqProvider(apiKey)

public fun defaultModel(): String = io.github.aaif_kaji.groqDefaultModel()
