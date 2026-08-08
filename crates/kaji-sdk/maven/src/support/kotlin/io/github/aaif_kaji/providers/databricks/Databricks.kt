package io.github.aaif_kaji.providers.databricks

public fun provider(host: String, token: String): io.github.aaif_kaji.Provider =
    io.github.aaif_kaji.databricksProvider(host, token)

public fun defaultModel(): String = io.github.aaif_kaji.databricksDefaultModel()
